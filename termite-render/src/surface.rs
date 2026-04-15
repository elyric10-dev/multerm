use std::sync::Arc;

use anyhow::Context;
use winit::window::Window;

/// Holds the wgpu device, queue, and configured surface.
pub struct GpuContext {
    pub device:          wgpu::Device,
    pub queue:           wgpu::Queue,
    pub surface:         wgpu::Surface<'static>,
    pub surface_config:  wgpu::SurfaceConfiguration,
    pub surface_format:  wgpu::TextureFormat,
}

impl GpuContext {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });

        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("create_surface")?;

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference:       wgpu::PowerPreference::HighPerformance,
                compatible_surface:     Some(&surface),
                force_fallback_adapter: false,
            },
        ))
        .context("no suitable GPU adapter found")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label:             Some("termite"),
                required_features: wgpu::Features::empty(),
                required_limits:   wgpu::Limits::default(),
                memory_hints:      wgpu::MemoryHints::default(),
            },
            None,
        ))
        .context("request_device")?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);

        // Prefer an sRGB format so we can use linear colour in shaders.
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage:                         wgpu::TextureUsages::RENDER_ATTACHMENT,
            format:                        surface_format,
            width:                         size.width.max(1),
            height:                        size.height.max(1),
            present_mode:                  wgpu::PresentMode::Fifo,
            alpha_mode:                    caps.alpha_modes[0],
            view_formats:                  vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        Ok(Self { device, queue, surface, surface_config, surface_format })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width  = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }
}
