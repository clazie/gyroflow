// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use qmetaobject::*;

use crate::{ core, rendering, util, controller::Controller };
use crate::core::{ undistortion, StabilizationManager };
use std::sync::{ Arc, atomic::{ AtomicBool, AtomicUsize, Ordering::SeqCst } };
use std::cell::RefCell;
use std::collections::HashMap;
use parking_lot::RwLock;

#[derive(Default, Clone, SimpleListItem)]
struct RenderQueueItem {
    pub job_id: u32,
    pub input_file: QString,
    pub output_path: QString,
    pub export_settings: QString,
    pub thumbnail_url: QString,
    pub current_frame: u64,
    pub total_frames: u64,
    pub start_timestamp: u64,
    pub end_timestamp: u64,
    pub error_string: QString,

    status: JobStatus,
}

#[derive(Clone, PartialEq)]
enum JobStatus {
    Queued,
    Rendering,
    Finished,
    Error
}
impl Default for JobStatus { fn default() -> Self { JobStatus::Queued }}
struct Job {
    queue_index: usize,
    input_file: String,
    render_options: RenderOptions,
    cancel_flag: Arc<AtomicBool>,
    stab: Arc<StabilizationManager<undistortion::RGBA8>>
}

#[derive(Default, Clone, serde::Deserialize)]
#[serde(default)]
pub struct RenderOptions {
    pub codec: String,
    pub codec_options: String,
    pub output_path: String,
    pub trim_start: f64,
    pub trim_end: f64,
    pub output_width: usize,
    pub output_height: usize,
    pub bitrate: f64,
    pub use_gpu: bool,
    pub audio: bool,
    pub pixel_format: String
}
impl RenderOptions {
    pub fn settings_string(&self, fps: f64) -> String {
        let codec_info = match self.codec.as_ref() {
            "x264" | "x265" => format!("{} {:.0} Mbps", self.codec, self.bitrate),
            "ProRes" => format!("{} {}", self.codec, self.codec_options),
            _ => self.codec.clone()
        };

        format!("{}x{} {:.3}fps | {}", self.output_width, self.output_height, fps, codec_info)
    }
}

#[derive(Default, QObject)]
pub struct RenderQueue { 
    base: qt_base_class!(trait QObject),  

    queue: qt_property!(RefCell<SimpleListModel<RenderQueueItem>>; NOTIFY queue_changed),
    jobs: HashMap<u32, Job>,
    
    add: qt_method!(fn(&mut self, controller: QJSValue, options_json: String, thumbnail_url: QString) -> u32),
    remove: qt_method!(fn(&mut self, job_id: u32)),

    start: qt_method!(fn(&mut self)),
    pause: qt_method!(fn(&mut self)),
    stop: qt_method!(fn(&mut self)),

    render_job: qt_method!(fn(&self, job_id: u32, single: bool)),
    cancel_job: qt_method!(fn(&self, job_id: u32)),
    reset_job: qt_method!(fn(&self, job_id: u32)),

    add_file: qt_method!(fn(&mut self, url: QUrl, controller: QJSValue, options_json: String) -> u32),

    get_job_output_path: qt_method!(fn(&self, job_id: u32) -> QString),

    set_pixel_format: qt_method!(fn(&mut self, job_id: u32, format: String)),

    main_job_id: qt_property!(u32),

    start_timestamp: qt_property!(u64; NOTIFY progress_changed),
    end_timestamp: qt_property!(u64; NOTIFY progress_changed),
    current_frame: qt_property!(u64; READ get_current_frame NOTIFY progress_changed),
    total_frames: qt_property!(u64; READ get_total_frames NOTIFY queue_changed),
    status: qt_property!(QString; NOTIFY status_changed),

    progress_changed: qt_signal!(),
    queue_changed: qt_signal!(),
    status_changed: qt_signal!(),
    
    render_progress: qt_signal!(job_id: u32, progress: f64, current_frame: usize, total_frames: usize, finished: bool),

    convert_format: qt_signal!(job_id: u32, format: QString, supported: QString),
    error: qt_signal!(job_id: u32, text: QString, arg: QString, callback: QString),
    added: qt_signal!(job_id: u32),

    pause_flag: Arc<AtomicBool>,

    paused_timestamp: Option<u64>
}

macro_rules! update_model {
    ($this:ident, $job_id:ident, $itm:ident $action:block) => {
        {
            let mut q = $this.queue.borrow_mut();
            if let Some(job) = $this.jobs.get(&$job_id) {
                if job.queue_index < q.row_count() as usize {
                    //let mut $itm = &mut q[job.queue_index];
                    let mut $itm = q[job.queue_index].clone();
                    $action
                    q.change_line(job.queue_index, $itm);
                    //q.data_changed(job.queue_index);
                }
            }
        }
    };
}

impl RenderQueue {
    pub fn new() -> Self {
        Self {
            status: QString::from("stopped"),
            ..Default::default()
        }
    }
    pub fn get_total_frames(&self) -> u64 {
        self.queue.borrow().iter().map(|v| v.total_frames).sum()
    }
    pub fn get_current_frame(&self) -> u64 {
        self.queue.borrow().iter().map(|v| v.current_frame).sum()
    }

    pub fn set_pixel_format(&mut self, job_id: u32, format: String) {
        if let Some(job) = self.jobs.get_mut(&job_id) {
            if format == "cpu" {
                job.render_options.use_gpu = false;
            } else {
                job.render_options.pixel_format = format;
            }
        }
        update_model!(self, job_id, itm {
            itm.error_string = QString::default();
            itm.status = JobStatus::Queued;
        });
        if self.status.to_string() != "active" {
            self.start();
        }
    }

    pub fn add(&mut self, controller: QJSValue, options_json: String, thumbnail_url: QString) -> u32 {
        let job_id = fastrand::u32(..);

        if let Some(ctl) = controller.to_qobject::<Controller>() {
            let ctl = unsafe { &mut *ctl.as_ptr() }; // ctl.borrow_mut()
            if let Ok(render_options) = serde_json::from_str(&options_json) as serde_json::Result<RenderOptions> {
                self.add_internal(job_id, ctl.stabilizer.clone(), render_options, thumbnail_url);
            }
        }
        job_id
    }

    pub fn add_internal(&mut self, job_id: u32, stab: Arc<StabilizationManager<undistortion::RGBA8>>, render_options: RenderOptions, thumbnail_url: QString) {
        let stab = Arc::new(stab.get_render_stabilizer((render_options.output_width, render_options.output_height)));
        let params = stab.params.read();
        let trim_ratio = render_options.trim_end - render_options.trim_start;
        let video_path = stab.video_path.read().clone();

        {
            let mut q = self.queue.borrow_mut();
            q.push(RenderQueueItem {
                job_id,
                input_file: QString::from(video_path.as_str()),
                output_path: QString::from(render_options.output_path.as_str()),
                export_settings: QString::from(render_options.settings_string(params.fps)),
                thumbnail_url,
                current_frame: 0,
                total_frames: (params.frame_count as f64 * trim_ratio).ceil() as u64,
                start_timestamp: 0,
                end_timestamp: 0,
                error_string: QString::default(),
                status: JobStatus::Queued,
            });
        }

        self.jobs.insert(job_id, Job {
            queue_index: 0,
            input_file: video_path,
            render_options,
            cancel_flag: Default::default(),
            stab: stab.clone()
        });
        self.update_queue_indices();

        self.queue_changed();
        self.added(job_id);
    }

    pub fn get_job_output_path(&self, job_id: u32) -> QString {
        let q = self.queue.borrow();
        if let Some(job) = self.jobs.get(&job_id) {
            if job.queue_index < q.row_count() as usize {
                return q[job.queue_index].output_path.clone();
            }
        }
        QString::default()
    }
    pub fn remove(&mut self, job_id: u32) {
        if let Some(job) = self.jobs.get(&job_id) {
            job.cancel_flag.store(true, SeqCst);
            self.queue.borrow_mut().remove(job.queue_index);
            self.queue_changed();
        }
        self.jobs.remove(&job_id);
        self.update_queue_indices();
    }
    fn update_queue_indices(&mut self) {
        for (i, v) in self.queue.borrow().iter().enumerate() {
            if let Some(job) = self.jobs.get_mut(&v.job_id) {
                job.queue_index = i;
            }
        }
    }
    fn current_timestamp() -> u64 {
        if let Ok(time) = std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH) {
            time.as_millis() as u64
        } else {
            0
        }
    }

    pub fn start(&mut self) {
        let paused = self.pause_flag.load(SeqCst) == true;

        for (_id, job) in self.jobs.iter() {
            job.cancel_flag.store(false, SeqCst);
        }
        self.pause_flag.store(false, SeqCst);

        self.status = QString::from("active");
        self.status_changed();

        if !paused && self.start_timestamp == 0 {
            self.start_timestamp = Self::current_timestamp();
            self.progress_changed();
        } else if let Some(paused_timestamp) = self.paused_timestamp.take() {
            let diff =  Self::current_timestamp() - paused_timestamp;
            self.start_timestamp += diff;
            let mut q = self.queue.borrow_mut();
            for i in 0..q.row_count() as usize {
                //let mut v = &mut q[i];
                let mut v = q[i].clone();
                if v.start_timestamp > 0 && v.current_frame < v.total_frames {
                    v.start_timestamp += diff;
                    //q.data_changed(i);
                    q.change_line(i, v);
                }
            }
        }
        
        if !paused {
            let mut job_id = None;
            for v in self.queue.borrow().iter() {
                if v.current_frame == 0 && v.total_frames > 0 && v.status == JobStatus::Queued {
                    job_id = Some(v.job_id);
                    break;
                }
            }
            if let Some(job_id) = job_id {
                self.render_job(job_id, false);
            } else {
                self.start_timestamp = 0;
                self.progress_changed();

                self.status = QString::from("stopped");
                self.status_changed();
            }
        }
    }
    pub fn pause(&mut self) {
        self.pause_flag.store(true, SeqCst);
        self.paused_timestamp = Some(Self::current_timestamp());

        self.status = QString::from("paused");
        self.status_changed();
    }
    pub fn stop(&mut self) {
        self.pause_flag.store(false, SeqCst);
        for (_id, job) in self.jobs.iter() {
            job.cancel_flag.store(true, SeqCst);
        }
        self.status = QString::from("stopped");
        self.status_changed();
    }
    pub fn cancel_job(&self, job_id: u32) {
        if let Some(job) = self.jobs.get(&job_id) {
            job.cancel_flag.store(true, SeqCst);
        }
    }
    pub fn reset_job(&self, job_id: u32) {
        if let Some(job) = self.jobs.get(&job_id) {
            job.cancel_flag.store(false, SeqCst);
        }
        update_model!(self, job_id, itm {
            itm.error_string = QString::default();
            itm.status = JobStatus::Queued;
        });
    }

    pub fn render_job(&self, job_id: u32, single: bool) {
        if let Some(job) = self.jobs.get(&job_id) {
            {
                let mut q = self.queue.borrow_mut();
                if job.queue_index < q.row_count() as usize {
                    //let mut itm = &mut q[job.queue_index];
                    let mut itm = q[job.queue_index].clone();
                    if itm.status == JobStatus::Rendering || itm.status == JobStatus::Finished {
                        ::log::warn!("Job is already rendering {}", job_id);
                        return;
                    }
                    itm.status = JobStatus::Rendering;
                    //q.data_changed(job.queue_index);
                    q.change_line(job.queue_index, itm);
                }
            }

            let stab = job.stab.clone();

            rendering::clear_log();

            let rendered_frames = Arc::new(AtomicUsize::new(0));
            let rendered_frames2 = rendered_frames.clone();
            let progress = util::qt_queued_callback_mut(self, move |this, (progress, current_frame, total_frames, finished): (f64, usize, usize, bool)| {
                rendered_frames2.store(current_frame, SeqCst);

                update_model!(this, job_id, itm {
                    itm.current_frame = current_frame as u64;
                    itm.total_frames = total_frames as u64;
                    if itm.start_timestamp == 0 {
                        itm.start_timestamp = Self::current_timestamp();
                    }
                    itm.end_timestamp = Self::current_timestamp();
                    if finished {
                        itm.status = JobStatus::Finished;
                    }
                });

                this.end_timestamp = Self::current_timestamp();
                this.render_progress(job_id, progress, current_frame, total_frames, finished);
                this.progress_changed();

                if finished && !single {
                    // Start the next one
                    this.start();
                }
            });

            let err = util::qt_queued_callback_mut(self, move |this, (msg, mut arg): (String, String)| {
                arg.push_str("\n\n");
                arg.push_str(&rendering::get_log());

                update_model!(this, job_id, itm {
                    itm.error_string = QString::from(arg.clone());
                    itm.status = JobStatus::Error;
                });

                this.error(job_id, QString::from(msg), QString::from(arg), QString::default());
                this.render_progress(job_id, 1.0, 0, 0, true);

                if !single {
                    // Start the next one
                    this.start();
                }
            });

            let convert_format = util::qt_queued_callback_mut(self, move |this, (format, mut supported): (String, String)| {
                use itertools::Itertools;
                supported = supported
                    .split(",")
                    .filter(|v| !["CUDA", "D3D11", "BGRZ", "RGBZ", "BGRA", "UYVY422", "VIDEOTOOLBOX", "DXVA2", "MEDIACODEC", "VULKAN", "OPENCL", "QSV"].contains(v))
                    .join(",");

                update_model!(this, job_id, itm {
                    itm.error_string = QString::from(format!("convert_format:{};{}", format, supported));
                    itm.status = JobStatus::Error;
                });

                this.convert_format(job_id, QString::from(format), QString::from(supported));
                this.render_progress(job_id, 1.0, 0, 0, true);

                if !single {
                    // Start the next one
                    this.start();
                }
            });
            let trim_ratio = job.render_options.trim_end - job.render_options.trim_start;
            let total_frame_count = stab.params.read().frame_count;
            let video_path = job.input_file.clone();
            let render_options = job.render_options.clone();

            progress((0.0, 0, (total_frame_count as f64 * trim_ratio).round() as usize, false));

            job.cancel_flag.store(false, SeqCst);
            let cancel_flag = job.cancel_flag.clone();
            let pause_flag = self.pause_flag.clone();

            let rendered_frames2 = rendered_frames.clone();
            core::run_threaded(move || {
                let mut i = 0;
                loop {
                    let result = rendering::render(stab.clone(), progress.clone(), &video_path, &render_options, i, cancel_flag.clone(), pause_flag.clone());
                    if let Err(e) = result {
                        if let rendering::FFmpegError::PixelFormatNotSupported((fmt, supported)) = e {
                            convert_format((format!("{:?}", fmt), supported.into_iter().map(|v| format!("{:?}", v)).collect::<Vec<String>>().join(",")));
                            break;
                        }
                        if rendered_frames2.load(SeqCst) == 0 {
                            if i >= 0 && i < 4 {
                                // Try 4 times with different GPU decoders
                                i += 1;
                                continue;
                            }
                            if i >= 0 && i < 5 {
                                // Try without GPU decoder
                                i = -1;
                                continue;
                            }
                        }
                        err(("An error occured: %1".to_string(), e.to_string()));
                        break;
                    } else {
                        // Render ok
                        break;
                    }
                }
            });
        }
    }

    fn get_output_path(path: &str, codec: &str) -> String {
        let mut path = std::path::Path::new(path).with_extension("");
        
        let ext = match codec {
            "ProRes"        => ".mov",
            "EXR Sequence"  => "_%05d.exr",
            "PNG Sequence"  => "_%05d.png",
            _ => ".mp4"
        };

        path.set_file_name(format!("{}_stabilized{}", path.file_name().map(|v| v.to_string_lossy()).unwrap_or_default(), ext));

        path.to_string_lossy().to_string()
    }

    pub fn add_file(&mut self, url: QUrl, controller: QJSValue, options_json: String) -> u32 {
        let job_id = fastrand::u32(..);

        let path = util::url_to_path(url);

        let err = util::qt_queued_callback_mut(self, move |this, (msg, arg): (String, String)| {
            ::log::warn!("[add_file]: {}", arg);
            update_model!(this, job_id, itm {
                itm.error_string = QString::from(arg.clone());
                itm.status = JobStatus::Error;
            });
            this.error(job_id, QString::from(msg), QString::from(arg), QString::default());
        });

        if let Some(ctl) = controller.to_qobject::<Controller>() {
            let ctl = unsafe { &mut *ctl.as_ptr() }; // ctl.borrow_mut()
            if let Ok(mut render_options) = serde_json::from_str(&options_json) as serde_json::Result<RenderOptions> {

                let (smoothing_name, smoothing_params) = {
                    let smoothing_lock = ctl.stabilizer.smoothing.read();
                    let smoothing = smoothing_lock.current();
                    (smoothing.get_name(), smoothing.get_parameters_json())
                };
                let params = ctl.stabilizer.params.read();

                let stab = StabilizationManager {
                    params: Arc::new(RwLock::new(core::stabilization_params::StabilizationParams {
                        fov:                    params.fov,
                        background:             params.background,
                        background_mode:        params.background_mode,
                        adaptive_zoom_window:   params.adaptive_zoom_window,
                        lens_correction_amount: params.lens_correction_amount,
                        ..Default::default()
                    })),
                    video_path: Arc::new(RwLock::new(path.clone())),
                    lens_profile_db: ctl.stabilizer.lens_profile_db.clone(),
                    ..Default::default()
                };

                {
                    let method_idx = stab.get_smoothing_algs()
                        .iter().enumerate()
                        .find(|(_, m)| smoothing_name == m.as_str())
                        .map(|(idx, _)| idx)
                        .unwrap_or_default();

                    let mut smoothing = stab.smoothing.write();
                    smoothing.set_current(method_idx);

                    for param in smoothing_params.as_array().unwrap() {
                        (|| -> Option<()> {
                            let name = param.get("name").and_then(|x| x.as_str())?;
                            let value = param.get("value").and_then(|x| x.as_f64())?;
                            smoothing.current_mut().set_parameter(name, value);
                            Some(())
                        })();
                    }
                }

                let stab = Arc::new(stab);

                let stab2 = stab.clone();
                let loaded = util::qt_queued_callback_mut(self, move |this, render_options: RenderOptions| {
                    this.add_internal(job_id, stab2.clone(), render_options, QString::default());
                });
                let thumb_fetched = util::qt_queued_callback_mut(self, move |this, thumb: QString| {
                    update_model!(this, job_id, itm { itm.thumbnail_url = thumb; });
                });

                core::run_threaded(move || {
                    let fetch_thumb = |video_path: &str, ratio: f64| -> Result<(), rendering::FFmpegError> {
                        let mut thumb = None;
                        {
                            let mut proc = rendering::FfmpegProcessor::from_file(video_path, false, 0)?;
                            proc.on_frame(|_timestamp_us, input_frame, _output_frame, converter| {
                                let sf = converter.scale(input_frame, ffmpeg_next::format::Pixel::RGBA, (50.0 * ratio).round() as u32, 50)?;
    
                                thumb = Some(util::image_data_to_base64(sf.plane_width(0), sf.plane_height(0), sf.stride(0) as u32, sf.data(0)));
    
                                Ok(())
                            });
                            proc.start_decoder_only(vec![(0.0, 0.0)], Arc::new(AtomicBool::new(false)))?;
                        }
                        if let Some(thumb) = thumb {
                            thumb_fetched(thumb);
                        }
                        Ok(())
                    };

                    if path.ends_with(".gyroflow") {
                        match stab.import_gyroflow(&path, true) {
                            Ok(obj) => {
                                if let Some(out) = obj.get("output") {
                                    if let Ok(render_options2) = serde_json::from_value(out.clone()) as serde_json::Result<RenderOptions> {
                                        loaded(render_options2);
                                    }
                                }
                                if let Some(out) = obj.get("videofile").and_then(|x| x.as_str()) {
                                    let ratio = {
                                        let params = stab.params.read();
                                        params.video_size.0 as f64 / params.video_size.1 as f64
                                    };

                                    if let Err(e) = fetch_thumb(out, ratio) {
                                        err(("An error occured: %1".to_string(), e.to_string()));
                                    }
                                }
                            },
                            Err(e) => {
                                ::log::error!("Error loading {}: {:?}", path, e);
                                return;
                            }
                        }
                    } else if let Ok(info) = rendering::FfmpegProcessor::get_video_info(&path) {
                        ::log::info!("Loaded {:?}", &info);

                        render_options.bitrate = info.bitrate;
                        render_options.output_width = info.width as usize;
                        render_options.output_height = info.height as usize;
                        render_options.output_path = Self::get_output_path(&path, &render_options.codec);
                        render_options.trim_start = 0.0;
                        render_options.trim_end = 1.0;

                        let ratio = info.width as f64 / info.height as f64;
        
                        if info.duration_ms > 0.0 && info.fps > 0.0 {

                            let video_size = (info.width as usize, info.height as usize);

                            if let Err(e) = stab.init_from_video_data(&path, info.duration_ms, info.fps, info.frame_count, video_size) {
                                err(("An error occured: %1".to_string(), e.to_string()));
                                return;
                            }
                            stab.set_size(video_size.0, video_size.1);
                            stab.set_output_size(video_size.0, video_size.1);
                            stab.recompute_blocking();
        
                            let camera_id = stab.camera_id.read();
        
                            let id_str = camera_id.as_ref().map(|v| v.identifier.clone()).unwrap_or_default();
                            if !id_str.is_empty() {
                                let db = stab.lens_profile_db.read();
                                if db.contains_id(&id_str) {
                                    if let Err(e) = stab.load_lens_profile(&id_str) {
                                        err(("An error occured: %1".to_string(), e.to_string()));
                                        return;
                                    }
                                }
                            }
                            if let Some(output_dim) = stab.lens.read().output_dimension.clone() {
                                render_options.output_width = output_dim.w;
                                render_options.output_height = output_dim.h;
                            }

                            // stab.export_gyroflow(format!("{}.gyroflow", path), false);

                            loaded(render_options);

                            if let Err(e) = fetch_thumb(&path, ratio) {
                                err(("An error occured: %1".to_string(), e.to_string()));
                            }
                        }
                    }
                });
            }
        }

        job_id
    }
}
