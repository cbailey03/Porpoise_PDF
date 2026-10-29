//! Drawing a frame to an image with no window: how a headless `capture` gets pixels.
//!
//! A window's screenshot is the frame it just presented. With no window there is
//! nothing presented, so this draws the frame again into a texture of our own and reads
//! it back. It uses the same renderer eframe does (`egui_wgpu`), reached through eframe
//! rather than as a dependency of its own, so the pixels match a window's.
//!
//! Its own module because it is the one part of headless mode that needs a GPU, and
//! keeping that here lets the rest of [`crate::headless`] run, and be tested, without
//! one.

use std::sync::{Arc, Mutex, mpsc};

use eframe::egui;
use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor, wgpu};

/// The format captures are drawn in.
///
/// The colour itself does not depend on this choice: egui-wgpu picks its shader's
/// output path from the target format, so any format it supports reads back the same
/// gamma-space bytes (tried: `Rgba8UnormSrgb` passes the colour test too). What depends
/// on it is the readback, which assumes four bytes per pixel in RGBA order. That is
/// also one of the two formats egui-wgpu's own screenshot code accepts.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// What a window clears to before egui draws: eframe 0.35's default `App::clear_color`.
fn clear_colour() -> wgpu::Color {
    let [r, g, b, a] =
        egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32();
    wgpu::Color {
        r: f64::from(r),
        g: f64::from(g),
        b: f64::from(b),
        a: f64::from(a),
    }
}

/// A GPU device and egui's renderer, with no surface: somewhere to draw frames to images.
pub(crate) struct Offscreen {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    /// The first GPU error since the last capture, if any. See [`watch_for_errors`].
    error: Arc<Mutex<Option<String>>>,
}

/// Routes the device's errors into `error` instead of wgpu's default, which panics.
///
/// A panic would end the whole headless session - capture or not - over one bad upload
/// or a lost device, where the spec asks that only the capture fail. So errors are kept
/// for [`Offscreen::capture`] to report, and logged as they happen.
fn watch_for_errors(device: &wgpu::Device) -> Arc<Mutex<Option<String>>> {
    let error = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&error);
    device.on_uncaptured_error(Arc::new(move |raised: wgpu::Error| {
        tracing::error!("headless: GPU error: {raised}");
        if let Ok(mut slot) = slot.lock() {
            slot.get_or_insert_with(|| raised.to_string());
        }
    }));
    let slot = Arc::clone(&error);
    device.set_device_lost_callback(move |reason, message| {
        tracing::error!("headless: GPU device lost ({reason:?}): {message}");
        if let Ok(mut slot) = slot.lock() {
            slot.get_or_insert_with(|| format!("the GPU device was lost: {message}"));
        }
    });
    error
}

impl Offscreen {
    /// Finds an adapter and sets up the renderer.
    ///
    /// A hardware adapter first, then a software one (Mesa's llvmpipe or lavapipe, WARP
    /// on Windows), because a headless process is exactly the kind that runs where there
    /// is no GPU. The error carries wgpu's reasons, for the person reading why capture
    /// was refused. `WGPU_BACKEND` and friends are honoured, as they are by eframe.
    pub(crate) fn new() -> Result<Self, String> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .or_else(|hardware| {
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    force_fallback_adapter: true,
                    ..wgpu::RequestAdapterOptions::default()
                }))
                .map_err(|software| format!("{hardware}; no software adapter either: {software}"))
            })?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("porpoise headless"),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|error| error.to_string())?;
        tracing::info!(
            "headless capture on {}",
            eframe::egui_wgpu::adapter_info_summary(&adapter.get_info())
        );

        let error = watch_for_errors(&device);
        let renderer = Renderer::new(&device, FORMAT, RendererOptions::default());
        let offscreen = Self {
            device,
            queue,
            renderer,
            error,
        };
        // A driver that cannot build egui's pipeline would otherwise surface on the first
        // capture. Failing here instead means capture is refused up front, with the reason.
        match offscreen.take_error() {
            Some(error) => Err(format!("egui's renderer could not be set up: {error}")),
            None => Ok(offscreen),
        }
    }

    /// The first GPU error since this was last asked, clearing it.
    fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }

    /// The largest texture side this device accepts, for egui's `max_texture_side`.
    pub(crate) fn max_texture_side(&self) -> usize {
        self.device.limits().max_texture_dimension_2d as usize
    }

    /// Uploads a frame's new and changed textures. Call before drawing that frame.
    pub(crate) fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        for (id, image) in &delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, image);
        }
    }

    /// Releases the textures a frame let go of. Call after drawing that frame.
    pub(crate) fn free_textures(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.renderer.free_texture(id);
        }
    }

    /// Draws `primitives` into a new image of `size_in_pixels`.
    ///
    /// Synchronous: it waits for the GPU and the copy back. That holds up one frame for
    /// a few milliseconds, which is fine for something done as rarely as a capture.
    pub(crate) fn capture(
        &mut self,
        primitives: &[egui::ClippedPrimitive],
        size_in_pixels: [u32; 2],
        pixels_per_point: f32,
    ) -> Result<egui::ColorImage, String> {
        let [width, height] = size_in_pixels;
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("porpoise capture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let screen = ScreenDescriptor {
            size_in_pixels,
            pixels_per_point,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let callback_buffers = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            primitives,
            &screen,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("porpoise capture"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear_colour()),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                // `Renderer::render` takes a `'static` pass; this is how egui-wgpu's own
                // window path gets one.
                .forget_lifetime();
            self.renderer.render(&mut pass, primitives, &screen);
        }

        // Rows in a texture-to-buffer copy must start on 256-byte boundaries, so each
        // row carries padding that is stripped again below.
        let row_bytes = width * 4;
        let padded_row_bytes = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("porpoise capture readback"),
            size: u64::from(padded_row_bytes) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes),
                    rows_per_image: Some(height),
                },
            },
            extent,
        );
        self.queue
            .submit(callback_buffers.into_iter().chain([encoder.finish()]));

        let slice = buffer.slice(..);
        let (sender, mapped) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            // The receiver outlives this call; a failed send means it already gave up.
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| format!("the GPU did not finish the capture: {error}"))?;
        mapped
            .recv()
            .map_err(|_| "the capture was never read back".to_owned())?
            .map_err(|error| format!("the capture could not be read back: {error}"))?;

        let mut rgba = Vec::with_capacity(row_bytes as usize * height as usize);
        {
            let padded = slice.get_mapped_range();
            for row in padded.chunks(padded_row_bytes as usize) {
                rgba.extend(row.iter().take(row_bytes as usize));
            }
        }
        buffer.unmap();

        // Checked last, so an error raised by this capture is caught as well as one left
        // over from an upload since the previous capture.
        if let Some(error) = self.take_error() {
            return Err(format!("the GPU reported an error: {error}"));
        }

        // Premultiplied, as egui-wgpu's own `read_screen_rgba` builds a window's.
        Ok(egui::ColorImage::from_rgba_premultiplied(
            [width as usize, height as usize],
            &rgba,
        ))
    }

    /// Whether the renderer holds `id`, for tests that check textures are released.
    #[cfg(test)]
    fn has_texture(&self, id: egui::TextureId) -> bool {
        self.renderer.texture(&id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, Color32, Pos2, Rect};

    /// The renderer, or `None` with a loud skip when this machine has no adapter at all.
    ///
    /// Loud because a test that quietly does nothing reads as coverage it is not
    /// providing. A machine with Mesa installed has a software adapter, so on most
    /// Linux machines these run even with no GPU.
    fn gpu(name: &str) -> Option<Offscreen> {
        match Offscreen::new() {
            Ok(offscreen) => Some(offscreen),
            Err(reason) => {
                eprintln!("SKIPPED {name}: no GPU or software renderer ({reason})");
                None
            }
        }
    }

    /// One frame of a bare context that fills a `size` screen with `fill`.
    fn filled_frame(
        size: [u32; 2],
        fill: Color32,
    ) -> (egui::TexturesDelta, Vec<egui::ClippedPrimitive>) {
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, egui::vec2(size[0] as f32, size[1] as f32));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..egui::RawInput::default()
            },
            |ui| {
                ui.painter().rect_filled(screen, 0.0, fill);
            },
        );
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        (output.textures_delta, primitives)
    }

    #[test]
    fn a_mid_tone_reads_back_exactly() {
        // Each choice here is what lets a readback bug show:
        // - three different channels, so swapped channel order is visible;
        // - mid-tones, because 0 and 255 read the same under sRGB and linear encoding;
        // - 50 pixels wide, because 200-byte rows need padding to 256 for the copy,
        //   and a padding bug only shows when there is padding to get wrong;
        // - the last pixel as well as the centre, because a row-stride bug drifts.
        // The tolerance of 1 is for egui's dithering, which assumes sRGB output.
        let Some(mut offscreen) = gpu("a_mid_tone_reads_back_exactly") else {
            return;
        };
        let size = [50, 40];
        let colour = Color32::from_rgb(200, 120, 60);
        let (textures, primitives) = filled_frame(size, colour);

        offscreen.set_textures(&textures);
        let image = offscreen
            .capture(&primitives, size, 1.0)
            .expect("the capture should succeed");

        assert_eq!(image.size, [50, 40]);
        for (at, pixel) in [
            ("centre", image.pixels[20 * 50 + 25]),
            ("last", image.pixels[40 * 50 - 1]),
        ] {
            let close = [(pixel.r(), 200), (pixel.g(), 120), (pixel.b(), 60)]
                .iter()
                .all(|&(got, want): &(u8, u8)| got.abs_diff(want) <= 1);
            assert!(
                close,
                "the {at} pixel came back as {pixel:?}, not {colour:?}"
            );
        }
    }

    #[test]
    fn the_capture_has_the_requested_size() {
        let Some(mut offscreen) = gpu("the_capture_has_the_requested_size") else {
            return;
        };
        let (textures, primitives) = filled_frame([1024, 768], Color32::WHITE);

        offscreen.set_textures(&textures);
        let image = offscreen
            .capture(&primitives, [1024, 768], 1.0)
            .expect("the capture should succeed");

        assert_eq!(image.size, [1024, 768]);
    }

    #[test]
    fn a_gpu_error_fails_the_capture_not_the_process() {
        // wgpu's default answer to an error nobody captured is to panic, which would end
        // a headless session over one bad upload - capture or not. A zero-sized texture
        // is a real validation error, raised the same way a lost device's would be.
        let Some(mut offscreen) = gpu("a_gpu_error_fails_the_capture_not_the_process") else {
            return;
        };
        let _ = offscreen.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("deliberately invalid"),
            size: wgpu::Extent3d {
                width: 0,
                height: 0,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let (textures, primitives) = filled_frame([64, 48], Color32::WHITE);
        offscreen.set_textures(&textures);

        let reason = offscreen
            .capture(&primitives, [64, 48], 1.0)
            .expect_err("a capture after a GPU error should report it");

        assert!(
            reason.contains("deliberately invalid"),
            "unhelpful: {reason}"
        );
        // Reported once, not forever: the next capture is judged on its own.
        assert!(offscreen.capture(&primitives, [64, 48], 1.0).is_ok());
    }

    #[test]
    fn freed_textures_leave_the_renderer() {
        // What keeps a long headless session from holding every page texture it ever
        // showed: the free half of a delta has to reach the GPU, not just the set half.
        let Some(mut offscreen) = gpu("freed_textures_leave_the_renderer") else {
            return;
        };
        let id = egui::TextureId::Managed(7);
        let set = egui::TexturesDelta {
            set: vec![(
                id,
                egui::epaint::ImageDelta::full(
                    egui::ColorImage::filled([1, 1], Color32::WHITE),
                    egui::TextureOptions::default(),
                ),
            )],
            free: Vec::new(),
        };
        let free = egui::TexturesDelta {
            set: Vec::new(),
            free: vec![id],
        };

        offscreen.set_textures(&set);
        assert!(offscreen.has_texture(id), "the texture never arrived");
        offscreen.free_textures(&free);

        assert!(!offscreen.has_texture(id), "the texture outlived its free");
    }
}
