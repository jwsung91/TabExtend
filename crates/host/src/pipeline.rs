//! The GStreamer pipeline: captured frames in, H.264 access units out.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;
use tabextend_core::encoder::EncoderSpec;
use tabextend_core::h264::low_delay_access_unit;

/// Element name the encoder gets inside the pipeline.
const ENCODER_NAME: &str = "encoder";

/// Element name of a PipeWire source whose frames carry the pointer as
/// metadata instead of in the picture. Captures that name their source this
/// get the pointer reported through `on_cursor`.
pub const CURSOR_SOURCE_NAME: &str = "capturesrc";

/// The pointer on the stream, as a fraction of its width and height, or
/// None when it is not on this monitor or hidden.
pub type CursorPosition = Option<(f32, f32)>;

/// Where pointer positions go, and the stream size they are relative to.
pub struct CursorSink {
    pub stream_size: (u32, u32),
    pub report: Box<dyn Fn(CursorPosition) + Send + Sync>,
}

/// Where a point of the captured picture lands in the stream, as a fraction
/// of the stream: the picture is scaled to fit and centred, with bars on two
/// sides if the shapes differ (as `videoscale add-borders=true` does).
pub fn cursor_on_stream(
    (x, y): (i32, i32),
    (source_w, source_h): (u32, u32),
    (stream_w, stream_h): (u32, u32),
) -> CursorPosition {
    if source_w == 0 || source_h == 0 || stream_w == 0 || stream_h == 0 {
        return None;
    }
    if x < 0 || y < 0 || x as u32 >= source_w || y as u32 >= source_h {
        return None;
    }
    let (sw, sh, tw, th) = (
        f64::from(source_w),
        f64::from(source_h),
        f64::from(stream_w),
        f64::from(stream_h),
    );
    let scale = (tw / sw).min(th / sh);
    let left = (tw - sw * scale) / 2.0;
    let top = (th - sh * scale) / 2.0;
    Some((
        ((left + f64::from(x) * scale) / tw) as f32,
        ((top + f64::from(y) * scale) / th) as f32,
    ))
}

/// The first of the spec's converters whose elements are all installed.
pub fn converter_for(spec: &EncoderSpec) -> &str {
    spec.converters
        .iter()
        .find(|fragment| {
            fragment
                .split('!')
                .map(|e| e.split_whitespace().next().unwrap_or(""))
                .all(|name| gst::ElementFactory::find(name).is_some())
        })
        .map_or("videoconvert", String::as_str)
}

/// Can this encoder chain take frames as GPU buffers (DMA-BUF) straight from
/// the compositor? Then no frame is copied through system memory. Only the
/// VA-API converter imports them; other chains get copied frames.
pub fn accepts_gpu_buffers(spec: &EncoderSpec) -> bool {
    converter_for(spec) == "vapostproc"
}

/// `converter ! encoder name=encoder`, for the middle of a pipeline.
pub fn encoder_fragment(spec: &EncoderSpec) -> String {
    format!(
        "{} ! {} name={ENCODER_NAME}",
        converter_for(spec),
        spec.element
    )
}

/// Set the encoder's properties, skipping ones this element version lacks.
/// Returns the settings that were skipped or rejected, for logging.
pub fn apply_settings(pipeline: &gst::Element, settings: &[(String, String)]) -> Vec<String> {
    let Some(encoder) = pipeline
        .downcast_ref::<gst::Bin>()
        .and_then(|bin| bin.by_name(ENCODER_NAME))
    else {
        return vec!["encoder element not found".into()];
    };
    let mut skipped = Vec::new();
    for (name, value) in settings {
        let Some(pspec) = encoder.find_property(name) else {
            skipped.push(format!("{name} (not supported)"));
            continue;
        };
        match gst::glib::Value::deserialize_with_pspec(value, &pspec) {
            Ok(parsed) => encoder.set_property_from_value(name, &parsed),
            Err(_) => skipped.push(format!("{name}={value} (rejected)")),
        }
    }
    skipped
}

/// One encoded access unit, Annex B, SPS/PPS included before keyframes.
pub struct EncodedFrame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub struct StreamConfig {
    pub bitrate_bps: u32,
    pub keyframe_interval: u32,
}

/// A running capture → encode pipeline.
pub struct EncodePipeline {
    pipeline: gst::Element,
    sink: gst::Element,
    captured: Arc<AtomicU64>,
}

impl EncodePipeline {
    /// Build and start. `on_frame` runs on GStreamer's streaming thread for
    /// every encoded frame; `on_error` once if the pipeline fails.
    pub fn start(
        source: &str,
        spec: &EncoderSpec,
        config: &StreamConfig,
        attach: impl FnOnce(&gst::Bin) -> Result<(), String>,
        cursor: CursorSink,
        on_frame: impl Fn(EncodedFrame) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        // queue leaky=downstream: if the encoder falls behind, drop raw
        // frames rather than build up latency in front of it.
        let description = format!(
            "{source} ! queue name=capture max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {} ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=2 drop=true",
            encoder_fragment(spec)
        );
        let pipeline = gst::parse::launch(&description).map_err(|e| e.to_string())?;
        attach(
            pipeline
                .downcast_ref::<gst::Bin>()
                .ok_or("pipeline is not a bin")?,
        )?;
        for skipped in apply_settings(
            &pipeline,
            &spec.settings(config.bitrate_bps, config.keyframe_interval),
        ) {
            eprintln!("note: {}: {skipped}", spec.element);
        }

        // Count frames as they arrive from the capture source, before any
        // dropping, to tell a slow compositor from a slow encoder.
        let captured = Arc::new(AtomicU64::new(0));
        if let Some(pad) = pipeline
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("capture"))
            .and_then(|queue| queue.static_pad("sink"))
        {
            let captured = captured.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                captured.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }

        if let Some(pad) = pipeline
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name(CURSOR_SOURCE_NAME))
            .and_then(|source| source.static_pad("src"))
        {
            add_cursor_probe(&pad, cursor);
        }

        // appsink through its signals, so no gstreamer-app development
        // package is needed to build TabExtend.
        let sink = pipeline
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("sink"))
            .ok_or("appsink missing")?;
        sink.set_property("emit-signals", true);
        sink.connect("new-sample", false, move |args| {
            let flow = (|| {
                let sink = args[0].get::<gst::Element>().ok()?;
                let sample = sink.emit_by_name::<Option<gst::Sample>>("pull-sample", &[])?;
                let buffer = sample.buffer()?;
                let map = buffer.map_readable().ok()?;
                let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                // Keyframes carry the SPS; make it promise low-latency
                // decoding so the tablet shows each frame at once.
                let data = keyframe
                    .then(|| low_delay_access_unit(map.as_slice()))
                    .flatten()
                    .unwrap_or_else(|| map.as_slice().to_vec());
                on_frame(EncodedFrame {
                    pts_us: buffer.pts().map_or(0, |t| t.useconds()),
                    keyframe,
                    data,
                });
                Some(gst::FlowReturn::Ok)
            })()
            .unwrap_or(gst::FlowReturn::Error);
            Some(flow.to_value())
        });

        let bus = pipeline.bus().ok_or("pipeline has no bus")?;
        let on_error = Arc::new(Mutex::new(Some(on_error)));
        bus.set_sync_handler(move |_, message| {
            if let gst::MessageView::Error(err) = message.view() {
                let text = match err.debug() {
                    Some(debug) => format!("{} ({debug})", err.error()),
                    None => err.error().to_string(),
                };
                if let Some(callback) = on_error.lock().ok().and_then(|mut f| f.take()) {
                    callback(text);
                }
            }
            gst::BusSyncReply::Drop
        });

        play(&pipeline, "video pipeline")?;
        Ok(Self {
            pipeline,
            sink,
            captured,
        })
    }

    /// Frames received from the capture source so far.
    pub fn captured_frames(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }

    /// Ask the encoder for a keyframe, with SPS/PPS, as soon as possible.
    pub fn request_keyframe(&self) {
        let event = gst::event::CustomUpstream::new(
            gst::Structure::builder("GstForceKeyUnit")
                .field("all-headers", true)
                .build(),
        );
        self.sink.send_event(event);
    }
}

/// Report the pointer from each captured buffer's metadata, and drop the
/// buffers that carry only a pointer move. With the pointer as metadata,
/// GNOME sends such a buffer whenever the pointer moves over a still screen;
/// it has no picture (PipeWire skips its empty chunk), so it must not reach
/// the encoder. A buffer without pointer metadata means the pointer is not
/// on this monitor: GNOME clears it then.
fn add_cursor_probe(pad: &gst::Pad, cursor: CursorSink) {
    let CursorSink {
        stream_size,
        report,
    } = cursor;
    let last: Mutex<Option<CursorPosition>> = Mutex::new(None);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let source_size = pad
            .current_caps()
            .and_then(|caps| gst_video::VideoInfo::from_caps(&caps).ok())
            .map(|info| (info.width(), info.height()));
        let position = buffer
            .iter_meta::<gst_video::VideoRegionOfInterestMeta>()
            .find(|meta| meta.roi_type() == "cursor")
            .and_then(|meta| {
                let (x, y, _, _) = meta.rect();
                cursor_on_stream((x as i32, y as i32), source_size?, stream_size)
            });
        if let Ok(mut last) = last.lock()
            && *last != Some(position)
        {
            *last = Some(position);
            report(position);
        }
        if buffer.n_memory() == 0 {
            gst::PadProbeReturn::Drop
        } else {
            gst::PadProbeReturn::Ok
        }
    });
}

/// Start a pipeline. On failure, say why (the reason is on the bus, the
/// state change error says nothing) and shut it down cleanly: a pipeline
/// dropped outside the NULL state leaks and makes GStreamer complain.
pub(crate) fn play(pipeline: &gst::Element, what: &str) -> Result<(), String> {
    if pipeline.set_state(gst::State::Playing).is_ok() {
        return Ok(());
    }
    let reason = pipeline
        .bus()
        .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
        .and_then(|message| match message.view() {
            gst::MessageView::Error(e) => Some(match e.debug() {
                Some(debug) => format!("{} ({debug})", e.error()),
                None => e.error().to_string(),
            }),
            _ => None,
        })
        .unwrap_or_else(|| "no reason given".to_owned());
    let _ = pipeline.set_state(gst::State::Null);
    Err(format!("{what} failed to start: {reason}"))
}

impl Drop for EncodePipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::cursor_on_stream;

    #[test]
    fn same_size_is_a_plain_fraction() {
        assert_eq!(
            cursor_on_stream((960, 300), (1920, 1200), (1920, 1200)),
            Some((0.5, 0.25))
        );
    }

    #[test]
    fn scaled_down_keeps_the_fraction() {
        assert_eq!(
            cursor_on_stream((1280, 800), (2560, 1600), (1920, 1200)),
            Some((0.5, 0.5))
        );
    }

    #[test]
    fn bars_shift_the_position() {
        // 1920×1080 fitted into 1920×1200: 60 px bars above and below.
        let (x, y) = cursor_on_stream((0, 0), (1920, 1080), (1920, 1200)).unwrap();
        assert_eq!(x, 0.0);
        assert!((y - 60.0 / 1200.0).abs() < 1e-6);
    }

    #[test]
    fn off_the_picture_is_hidden() {
        assert_eq!(cursor_on_stream((-1, 10), (1920, 1200), (1920, 1200)), None);
        assert_eq!(
            cursor_on_stream((1920, 10), (1920, 1200), (1920, 1200)),
            None
        );
        assert_eq!(cursor_on_stream((10, 10), (0, 0), (1920, 1200)), None);
    }
}
