//! One-frame VAAPI-to-DRM-PRIME acquisition for the GTK DMA-BUF preview.
//!
//! This module deliberately stops before presentation. It returns a mapped
//! DRM PRIME frame whose lifetime must extend until GDK releases the texture.

use std::ffi::{CStr, CString};
use std::os::fd::RawFd;
use std::ptr::{self, NonNull};

use ffmpeg_the_third::ffi;

const AVERROR_EAGAIN: i32 = -11; // Linux EAGAIN; VAAPI is Linux-only.
const HW_CONFIG_METHOD_DEVICE_CTX: i32 = 1;
const HWFRAME_MAP_READ: i32 = 1 << 0;
const HWFRAME_MAP_DIRECT: i32 = 1 << 3;
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");

#[derive(Clone, Copy, Debug)]
pub struct DmaBufPlane {
    pub fd: RawFd,
    pub stride: u32,
    pub offset: u32,
}

/// Owns an AVFrame and therefore the DMA-BUF reference stored in its descriptor.
///
/// GDK's release callback may run on a different thread, so this owner is
/// movable between threads. It is never accessed concurrently after transfer.
struct OwnedFrame(NonNull<ffi::AVFrame>);

unsafe impl Send for OwnedFrame {}

impl OwnedFrame {
    fn allocate() -> Result<Self, String> {
        let ptr = unsafe { ffi::av_frame_alloc() };
        NonNull::new(ptr)
            .map(Self)
            .ok_or_else(|| "FFmpeg could not allocate an AVFrame".to_string())
    }

    fn as_ptr(&self) -> *const ffi::AVFrame {
        self.0.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.0.as_ptr()
    }
}

impl Drop for OwnedFrame {
    fn drop(&mut self) {
        let mut ptr = self.0.as_ptr();
        unsafe { ffi::av_frame_free(&mut ptr) };
    }
}

/// A validated NV12 DMA-BUF frame. The file descriptor remains valid while
/// this value is alive because FFmpeg owns the mapped AVFrame reference.
pub struct VaapiDmaBufFrame {
    _owner: OwnedFrame,
    width: u32,
    height: u32,
    modifier: u64,
    planes: [DmaBufPlane; 2],
}

impl VaapiDmaBufFrame {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn fourcc(&self) -> u32 {
        DRM_FORMAT_NV12
    }

    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    pub fn plane(&self, index: usize) -> Option<DmaBufPlane> {
        self.planes.get(index).copied()
    }

    /// Keep the FFmpeg frame alive until GDK tells us the texture is released.
    pub fn into_release_callback(self) -> impl FnOnce() + Send + 'static {
        move || drop(self)
    }
}

struct FormatContext(*mut ffi::AVFormatContext);

impl Drop for FormatContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::avformat_close_input(&mut self.0) };
        }
    }
}

struct CodecContext(*mut ffi::AVCodecContext);

impl Drop for CodecContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::avcodec_free_context(&mut self.0) };
        }
    }
}

struct BufferRef(*mut ffi::AVBufferRef);

impl Drop for BufferRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}

struct Packet(*mut ffi::AVPacket);

impl Packet {
    fn allocate() -> Result<Self, String> {
        let ptr = unsafe { ffi::av_packet_alloc() };
        if ptr.is_null() {
            Err("FFmpeg could not allocate an AVPacket".into())
        } else {
            Ok(Self(ptr))
        }
    }

    fn unref(&mut self) {
        unsafe { ffi::av_packet_unref(self.0) };
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_packet_free(&mut self.0) };
        }
    }
}

fn ffmpeg_error(operation: &str, code: i32) -> String {
    let mut buffer = [0_i8; 128];
    let message = unsafe {
        let result = ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len());
        if result < 0 {
            format!("error code {code}")
        } else {
            CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
        }
    };
    format!("{operation}: {message} ({code})")
}

unsafe extern "C" fn select_vaapi_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }

    let mut current = formats;
    while *current != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *current == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return *current;
        }
        current = current.add(1);
    }

    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

fn validate_and_describe(
    mut mapped: OwnedFrame,
) -> Result<VaapiDmaBufFrame, String> {
    unsafe {
        let frame = &*mapped.as_ptr();
        if frame.format != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME.0 as i32
            || frame.data[0].is_null()
        {
            return Err("FFmpeg returned an invalid DRM PRIME frame".into());
        }

        let descriptor =
            &*(frame.data[0] as *const ffi::AVDRMFrameDescriptor);

        if descriptor.nb_objects != 1 || descriptor.nb_layers != 2 {
            return Err(format!(
                "Expected one DRM object and two layers; received {} objects and {} layers",
                descriptor.nb_objects, descriptor.nb_layers
            ));
        }

        let object = &descriptor.objects[0];
        let y_layer = &descriptor.layers[0];
        let uv_layer = &descriptor.layers[1];

        if object.fd < 0
            || y_layer.nb_planes != 1
            || uv_layer.nb_planes != 1
            || y_layer.format != DRM_FORMAT_R8
            || uv_layer.format != DRM_FORMAT_GR88
            || y_layer.planes[0].object_index != 0
            || uv_layer.planes[0].object_index != 0
        {
            return Err(
                "Only the single-object R8 + GR88 NV12 layout is supported".into(),
            );
        }

        let to_u32 = |value: isize, label: &str| {
            u32::try_from(value)
                .map_err(|_| format!("DRM {label} is negative or exceeds GDK's u32 range"))
        };

        let y_stride = to_u32(y_layer.planes[0].pitch, "Y-plane pitch")?;
        let uv_stride = to_u32(uv_layer.planes[0].pitch, "UV-plane pitch")?;
        let y_offset = to_u32(y_layer.planes[0].offset, "Y-plane offset")?;
        let uv_offset = to_u32(uv_layer.planes[0].offset, "UV-plane offset")?;

        if y_stride == 0 || uv_stride == 0 || frame.width <= 0 || frame.height <= 0 {
            return Err("DRM PRIME frame has invalid dimensions or plane strides".into());
        }

        let width = u32::try_from(frame.width)
            .map_err(|_| "DRM PRIME frame width is out of range".to_string())?;
        let height = u32::try_from(frame.height)
            .map_err(|_| "DRM PRIME frame height is out of range".to_string())?;

        Ok(VaapiDmaBufFrame {
            _owner: mapped,
            width,
            height,
            modifier: object.format_modifier,
            planes: [
                DmaBufPlane {
                    fd: object.fd,
                    stride: y_stride,
                    offset: y_offset,
                },
                DmaBufPlane {
                    fd: object.fd,
                    stride: uv_stride,
                    offset: uv_offset,
                },
            ],
        })
    }
}

fn codec_supports_vaapi(codec: *const ffi::AVCodec) -> bool {
    let mut config_index = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, config_index) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        if config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
            && (config.methods & HW_CONFIG_METHOD_DEVICE_CTX) != 0
        {
            return true;
        }
        config_index += 1;
    }
}

fn find_vaapi_decoder(codec_id: ffi::AVCodecID) -> *const ffi::AVCodec {
    let mut iterator: *mut std::ffi::c_void = ptr::null_mut();
    loop {
        let codec = unsafe { ffi::av_codec_iterate(&mut iterator) };
        if codec.is_null() {
            return ptr::null();
        }
        let matches = unsafe {
            ffi::av_codec_is_decoder(codec) != 0 && (*codec).id == codec_id
        };
        if matches && codec_supports_vaapi(codec) {
            return codec;
        }
    }
}

/// Decode and map the first video frame using VAAPI and direct DRM PRIME mapping.
///
/// This is a synchronous one-frame acquisition API, intended for the opt-in
/// preview experiment rather than the playback loop.
pub fn decode_first_vaapi_dmabuf(
    path: &str,
    render_node: &str,
) -> Result<VaapiDmaBufFrame, String> {
    let path = CString::new(path)
        .map_err(|_| "Media path contains an embedded NUL byte".to_string())?;
    let render_node = CString::new(render_node)
        .map_err(|_| "DRM render-node path contains an embedded NUL byte".to_string())?;

    let mut format = FormatContext(ptr::null_mut());
    let open_result = unsafe {
        ffi::avformat_open_input(
            &mut format.0,
            path.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if open_result < 0 {
        return Err(ffmpeg_error("Opening media", open_result));
    }

    let probe_result = unsafe {
        ffi::avformat_find_stream_info(format.0, ptr::null_mut())
    };
    if probe_result < 0 {
        return Err(ffmpeg_error("Reading stream information", probe_result));
    }

    let stream_index = unsafe {
        ffi::av_find_best_stream(
            format.0,
            ffi::AVMediaType::AVMEDIA_TYPE_VIDEO,
            -1,
            -1,
            ptr::null_mut(),
            0,
        )
    };
    if stream_index < 0 {
        return Err(ffmpeg_error("Finding video stream", stream_index));
    }

    let stream = unsafe { *(*format.0).streams.add(stream_index as usize) };
    if stream.is_null() || (*stream).codecpar.is_null() {
        return Err("Video stream has no codec parameters".into());
    }

    let codec_id = unsafe { (*(*stream).codecpar).codec_id };
    let codec = find_vaapi_decoder(codec_id);
    if codec.is_null() {
        return Err("No decoder with VAAPI device-context support is available for this codec".into());
    }

    let mut device_ptr = ptr::null_mut();
    let device_result = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ptr,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            render_node.as_ptr(),
            ptr::null_mut(),
            0,
        )
    };
    if device_result < 0 {
        return Err(ffmpeg_error("Creating VAAPI device", device_result));
    }
    let device = BufferRef(device_ptr);

    let mut decoder = CodecContext(unsafe { ffi::avcodec_alloc_context3(codec) });
    if decoder.0.is_null() {
        return Err("FFmpeg could not allocate a decoder context".into());
    }

    let params_result = unsafe {
        ffi::avcodec_parameters_to_context(decoder.0, (*stream).codecpar)
    };
    if params_result < 0 {
        return Err(ffmpeg_error("Copying video codec parameters", params_result));
    }

    let decoder_device = unsafe { ffi::av_buffer_ref(device.0) };
    if decoder_device.is_null() {
        return Err("FFmpeg could not reference the VAAPI device".into());
    }

    unsafe {
        (*decoder.0).hw_device_ctx = decoder_device;
        (*decoder.0).get_format = Some(select_vaapi_format);
    }

    let codec_open_result =
        unsafe { ffi::avcodec_open2(decoder.0, codec, ptr::null_mut()) };
    if codec_open_result < 0 {
        return Err(ffmpeg_error("Opening VAAPI decoder", codec_open_result));
    }

    let mut packet = Packet::allocate()?;
    let mut decoded = OwnedFrame::allocate()?;

    loop {
        let read_result = unsafe { ffi::av_read_frame(format.0, packet.0) };
        if read_result == ffi::AVERROR_EOF {
            break;
        }
        if read_result < 0 {
            return Err(ffmpeg_error("Reading media packet", read_result));
        }

        if unsafe { (*packet.0).stream_index } != stream_index {
            packet.unref();
            continue;
        }

        loop {
            let send_result =
                unsafe { ffi::avcodec_send_packet(decoder.0, packet.0) };
            if send_result == AVERROR_EAGAIN {
                let receive_result =
                    unsafe { ffi::avcodec_receive_frame(decoder.0, decoded.as_mut_ptr()) };
                if receive_result >= 0 {
                    packet.unref();
                    return map_vaapi_frame(decoded);
                }
                if receive_result != AVERROR_EAGAIN {
                    packet.unref();
                    return Err(ffmpeg_error("Receiving pending decoded frame", receive_result));
                }
                packet.unref();
                return Err("VAAPI decoder returned EAGAIN without a pending frame".into());
            }
            if send_result < 0 {
                packet.unref();
                return Err(ffmpeg_error("Sending video packet to decoder", send_result));
            }
            break;
        }

        packet.unref();

        let receive_result =
            unsafe { ffi::avcodec_receive_frame(decoder.0, decoded.as_mut_ptr()) };
        if receive_result >= 0 {
            return map_vaapi_frame(decoded);
        }
        if receive_result != AVERROR_EAGAIN {
            return Err(ffmpeg_error("Receiving decoded video frame", receive_result));
        }
    }

    let flush_result =
        unsafe { ffi::avcodec_send_packet(decoder.0, ptr::null()) };
    if flush_result < 0 && flush_result != ffi::AVERROR_EOF {
        if flush_result == AVERROR_EAGAIN {
            let receive_result =
                unsafe { ffi::avcodec_receive_frame(decoder.0, decoded.as_mut_ptr()) };
            if receive_result >= 0 {
                return map_vaapi_frame(decoded);
            }
        }
        return Err(ffmpeg_error("Flushing VAAPI decoder", flush_result));
    }

    let receive_result =
        unsafe { ffi::avcodec_receive_frame(decoder.0, decoded.as_mut_ptr()) };
    if receive_result < 0 {
        return Err(ffmpeg_error("Receiving flushed VAAPI frame", receive_result));
    }

    map_vaapi_frame(decoded)
}

fn map_vaapi_frame(decoded: OwnedFrame) -> Result<VaapiDmaBufFrame, String> {
    let mut mapped = OwnedFrame::allocate()?;
    unsafe {
        (*mapped.as_mut_ptr()).format =
            ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME.0 as i32;
    }

    let map_result = unsafe {
        ffi::av_hwframe_map(
            mapped.as_mut_ptr(),
            decoded.as_ptr(),
            HWFRAME_MAP_READ | HWFRAME_MAP_DIRECT,
        )
    };
    if map_result < 0 {
        return Err(ffmpeg_error("Mapping VAAPI frame to DRM PRIME", map_result));
    }

    validate_and_describe(mapped)
}
