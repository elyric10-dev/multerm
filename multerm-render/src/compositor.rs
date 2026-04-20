use std::mem;

use anyhow::Context;
use bytemuck::cast_slice;
use multerm_vt::{
    cell::WideKind,
    TerminalGrid,
};

use crate::{
    atlas::{GlyphAtlas, GlyphKey},
    color::color_to_linear,
    cursor::CursorState,
    gpu_types::{BgQuadInstance, GlyphInstance},
    selection::SelectionRange,
    surface::GpuContext,
};

/// Manages the wgpu pipelines and per-frame instance buffers.
pub struct Compositor {
    bg_pipeline:        wgpu::RenderPipeline,
    glyph_pipeline:     wgpu::RenderPipeline,

    uniform_buf:        wgpu::Buffer,
    uniform_bg:         wgpu::BindGroup,
    uniform_glyph:      wgpu::BindGroup,

    bg_buf:             wgpu::Buffer,
    bg_buf_cap:         usize,

    glyph_buf:          wgpu::Buffer,
    glyph_buf_cap:      usize,

    atlas_bg:           wgpu::BindGroup,
    atlas_bg_layout:    wgpu::BindGroupLayout,
}

const SHADER_BG:    &str = include_str!("../shaders/bg_quad.wgsl");
const SHADER_GLYPH: &str = include_str!("../shaders/glyph_quad.wgsl");

// Initial capacities (resized on demand)
const BG_INIT_CAP:    usize = 2048;
const GLYPH_INIT_CAP: usize = 2048;

impl Compositor {
    pub fn new(gpu: &GpuContext, atlas: &GlyphAtlas) -> anyhow::Result<Self> {
        let device = &gpu.device;

        // ── Uniform buffer (viewport size) ────────────────────────────────────
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label:              Some("uniforms"),
            size:               16,  // vec2<f32> + padding = 16 bytes
            usage:              wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label:   Some("uniform_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding:    0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty:         wgpu::BindingType::Buffer {
                    ty:                 wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size:   None,
                },
                count:      None,
            }],
        });
        let uniform_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label:   Some("uniform_bg"),
            layout:  &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding:  0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let uniform_glyph = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label:   Some("uniform_glyph"),
            layout:  &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding:  0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // ── Atlas bind group layout ───────────────────────────────────────────
        let atlas_bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label:   Some("atlas_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding:    0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty:         wgpu::BindingType::Texture {
                        sample_type:    wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled:   false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding:    1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty:         wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count:      None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label:             Some("atlas_sampler"),
            mag_filter:        wgpu::FilterMode::Linear,
            min_filter:        wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let atlas_bg = Self::make_atlas_bind_group(device, &atlas_bg_layout, &atlas.texture_view, &sampler);

        // ── BG pipeline ───────────────────────────────────────────────────────
        let bg_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label:  Some("bg_quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER_BG.into()),
        });
        let bg_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label:                Some("bg_layout"),
            bind_group_layouts:   &[&uniform_layout],
            push_constant_ranges: &[],
        });
        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label:   Some("bg_pipeline"),
            layout:  Some(&bg_pipeline_layout),
            vertex:  wgpu::VertexState {
                module:      &bg_shader,
                entry_point: Some("vs_main"),
                buffers:     &[wgpu::VertexBufferLayout {
                    array_stride: mem::size_of::<BgQuadInstance>() as u64,
                    step_mode:    wgpu::VertexStepMode::Instance,
                    attributes:   &wgpu::vertex_attr_array![
                        0 => Float32x4,
                        1 => Float32x4,
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module:      &bg_shader,
                entry_point: Some("fs_main"),
                targets:     &[Some(wgpu::ColorTargetState {
                    format:     gpu.surface_format,
                    blend:      Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive:    wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample:   wgpu::MultisampleState::default(),
            multiview:     None,
            cache:         None,
        });

        // ── Glyph pipeline ────────────────────────────────────────────────────
        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label:  Some("glyph_quad"),
            source: wgpu::ShaderSource::Wgsl(SHADER_GLYPH.into()),
        });
        let glyph_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label:                Some("glyph_layout"),
            bind_group_layouts:   &[&uniform_layout, &atlas_bg_layout],
            push_constant_ranges: &[],
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label:   Some("glyph_pipeline"),
            layout:  Some(&glyph_pipeline_layout),
            vertex:  wgpu::VertexState {
                module:      &glyph_shader,
                entry_point: Some("vs_main"),
                buffers:     &[wgpu::VertexBufferLayout {
                    array_stride: mem::size_of::<GlyphInstance>() as u64,
                    step_mode:    wgpu::VertexStepMode::Instance,
                    attributes:   &wgpu::vertex_attr_array![
                        0 => Float32x4,
                        1 => Float32x4,
                        2 => Float32x4,
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module:      &glyph_shader,
                entry_point: Some("fs_main"),
                targets:     &[Some(wgpu::ColorTargetState {
                    format:     gpu.surface_format,
                    blend:      Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive:    wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample:   wgpu::MultisampleState::default(),
            multiview:     None,
            cache:         None,
        });

        // ── Instance buffers ──────────────────────────────────────────────────
        let bg_buf = Self::make_instance_buf::<BgQuadInstance>(device, BG_INIT_CAP, "bg_instances");
        let glyph_buf = Self::make_instance_buf::<GlyphInstance>(device, GLYPH_INIT_CAP, "glyph_instances");

        Ok(Self {
            bg_pipeline,
            glyph_pipeline,
            uniform_buf,
            uniform_bg,
            uniform_glyph,
            bg_buf,
            bg_buf_cap: BG_INIT_CAP,
            glyph_buf,
            glyph_buf_cap: GLYPH_INIT_CAP,
            atlas_bg,
            atlas_bg_layout,
        })
    }

    // ── Main render call ──────────────────────────────────────────────────────

    /// Render a terminal frame.
    ///
    /// * `rect_px` — `[x, y, w, h]` pixel bounds of the terminal pane.
    /// * `scale`   — physical/logical pixel ratio (for atlas key).
    pub fn render_terminal_frame(
        &mut self,
        gpu: &GpuContext,
        atlas: &mut GlyphAtlas,
        scale: f32,
        rect_px: [f32; 4],
        grid: &TerminalGrid,
        selection: Option<SelectionRange>,
        cursor: Option<CursorState>,
    ) -> anyhow::Result<()> {
        self.render_terminal_panes(gpu, atlas, scale, &[(rect_px, grid)], &[selection], &[cursor])
    }

    /// Render multiple terminal panes in one frame.
    pub fn render_terminal_panes(
        &mut self,
        gpu: &GpuContext,
        atlas: &mut GlyphAtlas,
        scale: f32,
        panes: &[([f32; 4], &TerminalGrid)],
        selections: &[Option<SelectionRange>],
        cursors: &[Option<CursorState>],
    ) -> anyhow::Result<()> {
        if panes.is_empty() {
            return Ok(());
        }
        debug_assert!(
            selections.len() == panes.len(),
            "selections slice must match panes length"
        );
        debug_assert!(
            cursors.len() == panes.len(),
            "cursors slice must match panes length"
        );
        // Update atlas scale
        atlas.set_raster_scale(scale);

        let vp_w = gpu.surface_config.width  as f32;
        let vp_h = gpu.surface_config.height as f32;

        // Upload viewport uniform
        let uniform_data: [f32; 4] = [vp_w, vp_h, 0.0, 0.0];
        gpu.queue.write_buffer(&self.uniform_buf, 0, cast_slice(&uniform_data));

        let cell_w = atlas.cell_width();
        let cell_h = atlas.cell_height();

        let raster_scale_key = (scale * 100.0) as u32;

        // ── Build instance lists ──────────────────────────────────────────────
        let mut bg_insts:    Vec<BgQuadInstance> = Vec::new();
        let mut glyph_insts: Vec<GlyphInstance>  = Vec::new();

        for (pane_idx, (rect_px, grid)) in panes.iter().enumerate() {
            let origin_x = rect_px[0];
            let origin_y = rect_px[1];
            let selection = selections.get(pane_idx).copied().unwrap_or(None);
            let cursor = cursors
                .get(pane_idx)
                .copied()
                .flatten()
                .filter(|c| c.visible && c.row < grid.rows && c.col < grid.cols);

            for row in 0..grid.rows {
                for col in 0..grid.cols {
                    let cell = grid.cell(row, col);

                    // Skip trailing half of wide chars (renderer draws from leading half)
                    if cell.wide == WideKind::Trailing {
                        continue;
                    }

                    let is_selected = selection
                        .map_or(false, |sel| sel.contains(row, col, grid.rows, grid.cols));
                    let is_cursor_cell = cursor
                        .is_some_and(|cursor_state| cursor_state.row == row && cursor_state.col == col);

                    let px = origin_x + col as f32 * cell_w;
                    let py = origin_y + row as f32 * cell_h;
                    let gw = if cell.wide == WideKind::Leading { cell_w * 2.0 } else { cell_w };

                    let reverse = cell.attrs.contains(multerm_vt::CellAttrs::REVERSE);
                    let normal_fg = if reverse {
                        // Reverse: glyph foreground comes from the cell background.
                        color_to_linear(cell.bg, false)
                    } else {
                        color_to_linear(cell.fg, true)
                    };
                    let normal_bg = if reverse {
                        color_to_linear(cell.fg, true)
                    } else {
                        color_to_linear(cell.bg, false)
                    };

                    // Selection/cursor highlight: invert the effective fg/bg.
                    let (render_fg, render_bg) = if is_selected || is_cursor_cell {
                        (normal_bg, normal_fg)
                    } else {
                        (normal_fg, normal_bg)
                    };

                    // Background
                    let bg_color = render_bg;
                    bg_insts.push(BgQuadInstance {
                        rect:  [px, py, gw, cell_h],
                        color: bg_color,
                    });

                    // Glyph — skip space characters (no visible glyph)
                    if cell.ch != ' ' {
                        let key = GlyphKey {
                            ch:           cell.ch,
                            bold:         cell.attrs.contains(multerm_vt::CellAttrs::BOLD),
                            italic:       cell.attrs.contains(multerm_vt::CellAttrs::ITALIC),
                            raster_scale: raster_scale_key,
                        };

                        if let Some(uv) = atlas.get_or_rasterize(gpu, key) {
                            glyph_insts.push(GlyphInstance {
                                dest_rect: [px, py, uv.px_w as f32, uv.px_h as f32],
                                src_rect:  [uv.u, uv.v, uv.uw, uv.vh],
                                color:     render_fg,
                            });
                        }
                    }
                }
            }
        }

        // ── Upload instance data ──────────────────────────────────────────────
        self.ensure_bg_buf(gpu, bg_insts.len());
        self.ensure_glyph_buf(gpu, glyph_insts.len());

        if !bg_insts.is_empty() {
            gpu.queue.write_buffer(&self.bg_buf, 0, cast_slice(&bg_insts));
        }
        if !glyph_insts.is_empty() {
            gpu.queue.write_buffer(&self.glyph_buf, 0, cast_slice(&glyph_insts));
        }

        // Rebuild atlas bind group (texture view ref stays valid across calls)
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label:      Some("atlas_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        self.atlas_bg = Self::make_atlas_bind_group(
            &gpu.device, &self.atlas_bg_layout, &atlas.texture_view, &sampler
        );

        // ── Render pass ───────────────────────────────────────────────────────
        let frame  = gpu.surface.get_current_texture().context("get_current_texture")?;
        let view   = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = gpu.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("multerm_frame") }
        );

        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view:           &view,
                    resolve_target: None,
                    ops:            wgpu::Operations {
                        load:  wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0, g: 0.0, b: 0.0, a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes:         None,
                occlusion_query_set:      None,
            });

            // Pass 1: backgrounds
            if !bg_insts.is_empty() {
                rp.set_pipeline(&self.bg_pipeline);
                rp.set_bind_group(0, &self.uniform_bg, &[]);
                rp.set_vertex_buffer(0, self.bg_buf.slice(..));
                rp.draw(0..6, 0..bg_insts.len() as u32);
            }

            // Pass 2: glyphs
            if !glyph_insts.is_empty() {
                rp.set_pipeline(&self.glyph_pipeline);
                rp.set_bind_group(0, &self.uniform_glyph, &[]);
                rp.set_bind_group(1, &self.atlas_bg, &[]);
                rp.set_vertex_buffer(0, self.glyph_buf.slice(..));
                rp.draw(0..6, 0..glyph_insts.len() as u32);
            }
        }

        gpu.queue.submit(std::iter::once(enc.finish()));
        frame.present();
        Ok(())
    }

    // ── Buffer helpers ────────────────────────────────────────────────────────

    fn ensure_bg_buf(&mut self, gpu: &GpuContext, need: usize) {
        if need > self.bg_buf_cap {
            let new_cap = (need * 2).max(self.bg_buf_cap * 2);
            self.bg_buf     = Self::make_instance_buf::<BgQuadInstance>(&gpu.device, new_cap, "bg_instances");
            self.bg_buf_cap = new_cap;
        }
    }

    fn ensure_glyph_buf(&mut self, gpu: &GpuContext, need: usize) {
        if need > self.glyph_buf_cap {
            let new_cap = (need * 2).max(self.glyph_buf_cap * 2);
            self.glyph_buf     = Self::make_instance_buf::<GlyphInstance>(&gpu.device, new_cap, "glyph_instances");
            self.glyph_buf_cap = new_cap;
        }
    }

    fn make_instance_buf<T>(device: &wgpu::Device, cap: usize, label: &str) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label:              Some(label),
            size:               (mem::size_of::<T>() * cap.max(1)) as u64,
            usage:              wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn make_atlas_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        view:   &wgpu::TextureView,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label:   Some("atlas_bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        })
    }
}
