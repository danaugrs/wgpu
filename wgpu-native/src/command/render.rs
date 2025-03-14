use crate::{
    command::bind::{Binder, LayoutChange},
    conv,
    device::{RenderPassContext, BIND_BUFFER_ALIGNMENT, MAX_VERTEX_BUFFERS},
    gfx_select,
    hub::{GfxBackend, Token},
    pipeline::{IndexFormat, InputStepMode, PipelineFlags},
    resource::BufferUsage,
    track::{Stitch, TrackerSet},
    BindGroupId,
    BufferAddress,
    BufferId,
    Color,
    CommandBuffer,
    CommandBufferId,
    RawString,
    RenderBundleId,
    RenderPassId,
    RenderPipelineId,
    Stored,
};

use hal::command::RawCommandBuffer;
use log::trace;

use std::{iter, ops::Range, slice};

#[derive(Debug, PartialEq)]
enum OptionalState {
    Unused,
    Required,
    Set,
}

impl OptionalState {
    fn require(&mut self, require: bool) {
        if require && *self == OptionalState::Unused {
            *self = OptionalState::Required;
        }
    }
}

#[derive(Debug, PartialEq)]
enum DrawError {
    MissingBlendColor,
    MissingStencilReference,
    IncompatibleBindGroup {
        index: u32,
        //expected: BindGroupLayoutId,
        //provided: Option<(BindGroupLayoutId, BindGroupId)>,
    },
}

#[derive(Debug)]
pub struct IndexState {
    bound_buffer_view: Option<(BufferId, Range<BufferAddress>)>,
    format: IndexFormat,
    limit: u32,
}

impl IndexState {
    fn update_limit(&mut self) {
        self.limit = match self.bound_buffer_view {
            Some((_, ref range)) => {
                let shift = match self.format {
                    IndexFormat::Uint16 => 1,
                    IndexFormat::Uint32 => 2,
                };
                ((range.end - range.start) >> shift) as u32
            }
            None => 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VertexBufferState {
    total_size: BufferAddress,
    stride: BufferAddress,
    rate: InputStepMode,
}

impl VertexBufferState {
    const EMPTY: Self = VertexBufferState {
        total_size: 0,
        stride: 0,
        rate: InputStepMode::Vertex,
    };
}

#[derive(Debug)]
pub struct VertexState {
    inputs: [VertexBufferState; MAX_VERTEX_BUFFERS],
    vertex_limit: u32,
    instance_limit: u32,
}

impl VertexState {
    fn update_limits(&mut self) {
        self.vertex_limit = !0;
        self.instance_limit = !0;
        for vbs in &self.inputs {
            if vbs.stride == 0 {
                continue;
            }
            let limit = (vbs.total_size / vbs.stride) as u32;
            match vbs.rate {
                InputStepMode::Vertex => self.vertex_limit = self.vertex_limit.min(limit),
                InputStepMode::Instance => self.instance_limit = self.instance_limit.min(limit),
            }
        }
    }
}

#[derive(Debug)]
pub struct RenderPass<B: hal::Backend> {
    raw: B::CommandBuffer,
    cmb_id: Stored<CommandBufferId>,
    context: RenderPassContext,
    binder: Binder,
    trackers: TrackerSet,
    blend_color_status: OptionalState,
    stencil_reference_status: OptionalState,
    index_state: IndexState,
    vertex_state: VertexState,
    sample_count: u8,
}

impl<B: GfxBackend> RenderPass<B> {
    pub(crate) fn new(
        raw: B::CommandBuffer,
        cmb_id: Stored<CommandBufferId>,
        context: RenderPassContext,
        sample_count: u8,
    ) -> Self {
        RenderPass {
            raw,
            cmb_id,
            context,
            binder: Binder::default(),
            trackers: TrackerSet::new(B::VARIANT),
            blend_color_status: OptionalState::Unused,
            stencil_reference_status: OptionalState::Unused,
            index_state: IndexState {
                bound_buffer_view: None,
                format: IndexFormat::Uint16,
                limit: 0,
            },
            vertex_state: VertexState {
                inputs: [VertexBufferState::EMPTY; MAX_VERTEX_BUFFERS],
                vertex_limit: 0,
                instance_limit: 0,
            },
            sample_count,
        }
    }

    fn is_ready(&self) -> Result<(), DrawError> {
        //TODO: vertex buffers
        let bind_mask = self.binder.invalid_mask();
        if bind_mask != 0 {
            //let (expected, provided) = self.binder.entries[index as usize].info();
            return Err(DrawError::IncompatibleBindGroup {
                index: bind_mask.trailing_zeros() as u32,
            });
        }
        if self.blend_color_status == OptionalState::Required {
            return Err(DrawError::MissingBlendColor);
        }
        if self.stencil_reference_status == OptionalState::Required {
            return Err(DrawError::MissingStencilReference);
        }
        Ok(())
    }
}

// Common routines between render/compute

pub fn render_pass_end_pass<B: GfxBackend>(pass_id: RenderPassId) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut cmb_guard, mut token) = hub.command_buffers.write(&mut token);
    let (mut pass, mut token) = hub.render_passes.unregister(pass_id, &mut token);
    unsafe {
        pass.raw.end_render_pass();
    }
    pass.trackers.optimize();
    let cmb = &mut cmb_guard[pass.cmb_id.value];
    let (buffer_guard, mut token) = hub.buffers.read(&mut token);
    let (texture_guard, _) = hub.textures.read(&mut token);

    match cmb.raw.last_mut() {
        Some(last) => {
            trace!("Encoding barriers before pass {:?}", pass_id);
            CommandBuffer::insert_barriers(
                last,
                &mut cmb.trackers,
                &pass.trackers,
                Stitch::Last,
                &*buffer_guard,
                &*texture_guard,
            );
            unsafe { last.finish() };
        }
        None => {
            cmb.trackers.merge_extend(&pass.trackers);
        }
    }

    cmb.raw.push(pass.raw);
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_end_pass(pass_id: RenderPassId) {
    gfx_select!(pass_id => render_pass_end_pass(pass_id))
}

pub fn render_pass_set_bind_group<B: GfxBackend>(
    pass_id: RenderPassId,
    index: u32,
    bind_group_id: BindGroupId,
    offsets: &[BufferAddress],
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);

    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    let bind_group = pass
        .trackers
        .bind_groups
        .use_extend(&*bind_group_guard, bind_group_id, (), ())
        .unwrap();

    assert_eq!(bind_group.dynamic_count, offsets.len());

    if cfg!(debug_assertions) {
        for off in offsets {
            assert_eq!(
                *off % BIND_BUFFER_ALIGNMENT,
                0,
                "Misaligned dynamic buffer offset: {} does not align with {}",
                off,
                BIND_BUFFER_ALIGNMENT
            );
        }
    }

    pass.trackers.merge_extend(&bind_group.used);

    if let Some((pipeline_layout_id, follow_up_sets, follow_up_offsets)) = pass
        .binder
        .provide_entry(index as usize, bind_group_id, bind_group, offsets)
    {
        let bind_groups = iter::once(bind_group.raw.raw())
            .chain(follow_up_sets.map(|bg_id| bind_group_guard[bg_id].raw.raw()));
        unsafe {
            pass.raw.bind_graphics_descriptor_sets(
                &&pipeline_layout_guard[pipeline_layout_id].raw,
                index as usize,
                bind_groups,
                offsets
                    .iter()
                    .chain(follow_up_offsets)
                    .map(|&off| off as hal::command::DescriptorSetOffset),
            );
        }
    };
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_bind_group(
    pass_id: RenderPassId,
    index: u32,
    bind_group_id: BindGroupId,
    offsets: *const BufferAddress,
    offsets_length: usize,
) {
    let offsets = if offsets_length != 0 {
        unsafe { slice::from_raw_parts(offsets, offsets_length) }
    } else {
        &[]
    };
    gfx_select!(pass_id => render_pass_set_bind_group(pass_id, index, bind_group_id, offsets))
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_push_debug_group(_pass_id: RenderPassId, _label: RawString) {
    //TODO
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_pop_debug_group(_pass_id: RenderPassId) {
    //TODO
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_insert_debug_marker(_pass_id: RenderPassId, _label: RawString) {
    //TODO
}

// Render-specific routines

pub fn render_pass_set_index_buffer<B: GfxBackend>(
    pass_id: RenderPassId,
    buffer_id: BufferId,
    offset: BufferAddress,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, mut token) = hub.render_passes.write(&mut token);
    let (buffer_guard, _) = hub.buffers.read(&mut token);

    let pass = &mut pass_guard[pass_id];
    let buffer = pass
        .trackers
        .buffers
        .use_extend(&*buffer_guard, buffer_id, (), BufferUsage::INDEX)
        .unwrap();

    let range = offset .. buffer.size;
    pass.index_state.bound_buffer_view = Some((buffer_id, range));
    pass.index_state.update_limit();

    let view = hal::buffer::IndexBufferView {
        buffer: &buffer.raw,
        offset,
        index_type: conv::map_index_format(pass.index_state.format),
    };

    unsafe {
        pass.raw.bind_index_buffer(view);
    }
}

pub extern "C" fn wgpu_render_pass_set_index_buffer(
    pass_id: RenderPassId,
    buffer_id: BufferId,
    offset: BufferAddress,
) {
    gfx_select!(pass_id => render_pass_set_index_buffer(pass_id, buffer_id, offset))
}

pub fn render_pass_set_vertex_buffers<B: GfxBackend>(
    pass_id: RenderPassId,
    start_slot: u32,
    buffers: &[BufferId],
    offsets: &[BufferAddress],
) {
    let hub = B::hub();
    let mut token = Token::root();
    assert_eq!(buffers.len(), offsets.len());

    let (mut pass_guard, mut token) = hub.render_passes.write(&mut token);
    let (buffer_guard, _) = hub.buffers.read(&mut token);

    let pass = &mut pass_guard[pass_id];
    for (vbs, (&id, &offset)) in pass.vertex_state.inputs[start_slot as usize ..]
        .iter_mut()
        .zip(buffers.iter().zip(offsets))
    {
        let buffer = pass
            .trackers
            .buffers
            .use_extend(&*buffer_guard, id, (), BufferUsage::VERTEX)
            .unwrap();
        vbs.total_size = buffer.size - offset;
    }

    pass.vertex_state.update_limits();

    let buffers = buffers
        .iter()
        .map(|&id| &buffer_guard[id].raw)
        .zip(offsets.iter().cloned());

    unsafe {
        pass.raw.bind_vertex_buffers(start_slot, buffers);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_vertex_buffers(
    pass_id: RenderPassId,
    start_slot: u32,
    buffers: *const BufferId,
    offsets: *const BufferAddress,
    length: usize,
) {
    let buffers = unsafe { slice::from_raw_parts(buffers, length) };
    let offsets = unsafe { slice::from_raw_parts(offsets, length) };
    gfx_select!(pass_id => render_pass_set_vertex_buffers(pass_id, start_slot, buffers, offsets))
}

pub fn render_pass_draw<B: GfxBackend>(
    pass_id: RenderPassId,
    vertex_count: u32,
    instance_count: u32,
    first_vertex: u32,
    first_instance: u32,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];
    pass.is_ready().unwrap();

    assert!(
        first_vertex + vertex_count <= pass.vertex_state.vertex_limit,
        "Vertex out of range!"
    );
    assert!(
        first_instance + instance_count <= pass.vertex_state.instance_limit,
        "Instance out of range!"
    );

    unsafe {
        pass.raw.draw(
            first_vertex .. first_vertex + vertex_count,
            first_instance .. first_instance + instance_count,
        );
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_draw(
    pass_id: RenderPassId,
    vertex_count: u32,
    instance_count: u32,
    first_vertex: u32,
    first_instance: u32,
) {
    gfx_select!(pass_id => render_pass_draw(pass_id, vertex_count, instance_count, first_vertex, first_instance))
}

pub fn render_pass_draw_indirect<B: GfxBackend>(
    pass_id: RenderPassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let (buffer_guard, _) = hub.buffers.read(&mut token);
    let pass = &mut pass_guard[pass_id];
    pass.is_ready().unwrap();

    let buffer = pass
        .trackers
        .buffers
        .use_extend(
            &*buffer_guard,
            indirect_buffer_id,
            (),
            BufferUsage::INDIRECT,
        )
        .unwrap();

    unsafe {
        pass.raw.draw_indirect(&buffer.raw, indirect_offset, 1, 0);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_draw_indirect(
    pass_id: RenderPassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    gfx_select!(pass_id => render_pass_draw_indirect(pass_id, indirect_buffer_id, indirect_offset))
}

pub fn render_pass_draw_indexed<B: GfxBackend>(
    pass_id: RenderPassId,
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];
    pass.is_ready().unwrap();

    //TODO: validate that base_vertex + max_index() is within the provided range
    assert!(
        first_index + index_count <= pass.index_state.limit,
        "Index out of range!"
    );
    assert!(
        first_instance + instance_count <= pass.vertex_state.instance_limit,
        "Instance out of range!"
    );

    unsafe {
        pass.raw.draw_indexed(
            first_index .. first_index + index_count,
            base_vertex,
            first_instance .. first_instance + instance_count,
        );
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_draw_indexed(
    pass_id: RenderPassId,
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
) {
    gfx_select!(pass_id => render_pass_draw_indexed(pass_id, index_count, instance_count, first_index, base_vertex, first_instance))
}

pub fn render_pass_draw_indexed_indirect<B: GfxBackend>(
    pass_id: RenderPassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let (buffer_guard, _) = hub.buffers.read(&mut token);
    let pass = &mut pass_guard[pass_id];
    pass.is_ready().unwrap();

    let buffer = pass
        .trackers
        .buffers
        .use_extend(
            &*buffer_guard,
            indirect_buffer_id,
            (),
            BufferUsage::INDIRECT,
        )
        .unwrap();

    unsafe {
        pass.raw
            .draw_indexed_indirect(&buffer.raw, indirect_offset, 1, 0);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_draw_indexed_indirect(
    pass_id: RenderPassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    gfx_select!(pass_id => render_pass_draw_indexed_indirect(pass_id, indirect_buffer_id, indirect_offset))
}

pub fn render_pass_set_pipeline<B: GfxBackend>(
    pass_id: RenderPassId,
    pipeline_id: RenderPipelineId,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
    let (mut pass_guard, mut token) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];
    let (pipeline_guard, mut token) = hub.render_pipelines.read(&mut token);
    let pipeline = &pipeline_guard[pipeline_id];

    assert!(
        pass.context.compatible(&pipeline.pass_context),
        "The render pipeline is not compatible with the pass!"
    );
    assert_eq!(
        pipeline.sample_count, pass.sample_count,
        "The render pipeline and renderpass have mismatching sample_count"
    );

    pass.blend_color_status
        .require(pipeline.flags.contains(PipelineFlags::BLEND_COLOR));
    pass.stencil_reference_status
        .require(pipeline.flags.contains(PipelineFlags::STENCIL_REFERENCE));

    unsafe {
        pass.raw.bind_graphics_pipeline(&pipeline.raw);
    }

    // Rebind resource
    if pass.binder.pipeline_layout_id != Some(pipeline.layout_id.clone()) {
        let pipeline_layout = &pipeline_layout_guard[pipeline.layout_id];
        pass.binder.pipeline_layout_id = Some(pipeline.layout_id.clone());
        pass.binder
            .reset_expectations(pipeline_layout.bind_group_layout_ids.len());
        let mut is_compatible = true;

        for (index, (entry, &bgl_id)) in pass
            .binder
            .entries
            .iter_mut()
            .zip(&pipeline_layout.bind_group_layout_ids)
            .enumerate()
        {
            match entry.expect_layout(bgl_id) {
                LayoutChange::Match(bg_id, offsets) if is_compatible => {
                    let desc_set = bind_group_guard[bg_id].raw.raw();
                    unsafe {
                        pass.raw.bind_graphics_descriptor_sets(
                            &pipeline_layout.raw,
                            index,
                            iter::once(desc_set),
                            offsets.iter().map(|offset| *offset as u32),
                        );
                    }
                }
                LayoutChange::Match(..) |
                LayoutChange::Unchanged => {}
                LayoutChange::Mismatch => {
                    is_compatible = false;
                }
            }
        }
    }

    // Rebind index buffer if the index format has changed with the pipeline switch
    if pass.index_state.format != pipeline.index_format {
        pass.index_state.format = pipeline.index_format;
        pass.index_state.update_limit();

        if let Some((buffer_id, ref range)) = pass.index_state.bound_buffer_view {
            let (buffer_guard, _) = hub.buffers.read(&mut token);
            let buffer = pass
                .trackers
                .buffers
                .use_extend(&*buffer_guard, buffer_id, (), BufferUsage::INDEX)
                .unwrap();

            let view = hal::buffer::IndexBufferView {
                buffer: &buffer.raw,
                offset: range.start,
                index_type: conv::map_index_format(pass.index_state.format),
            };

            unsafe {
                pass.raw.bind_index_buffer(view);
            }
        }
    }
    // Update vertex buffer limits
    for (vbs, &(stride, rate)) in pass
        .vertex_state
        .inputs
        .iter_mut()
        .zip(&pipeline.vertex_strides)
    {
        vbs.stride = stride;
        vbs.rate = rate;
    }
    for vbs in pass.vertex_state.inputs[pipeline.vertex_strides.len() ..].iter_mut() {
        vbs.stride = 0;
        vbs.rate = InputStepMode::Vertex;
    }
    pass.vertex_state.update_limits();
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_pipeline(
    pass_id: RenderPassId,
    pipeline_id: RenderPipelineId,
) {
    gfx_select!(pass_id => render_pass_set_pipeline(pass_id, pipeline_id))
}

pub fn render_pass_set_blend_color<B: GfxBackend>(pass_id: RenderPassId, color: &Color) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    pass.blend_color_status = OptionalState::Set;

    unsafe {
        pass.raw.set_blend_constants(conv::map_color_f32(color));
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_blend_color(pass_id: RenderPassId, color: &Color) {
    gfx_select!(pass_id => render_pass_set_blend_color(pass_id, color))
}

pub fn render_pass_set_stencil_reference<B: GfxBackend>(pass_id: RenderPassId, value: u32) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    pass.stencil_reference_status = OptionalState::Set;

    unsafe {
        pass.raw.set_stencil_reference(hal::pso::Face::all(), value);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_stencil_reference(pass_id: RenderPassId, value: u32) {
    gfx_select!(pass_id => render_pass_set_stencil_reference(pass_id, value))
}

pub fn render_pass_set_viewport<B: GfxBackend>(
    pass_id: RenderPassId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    min_depth: f32,
    max_depth: f32,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    unsafe {
        use std::convert::TryFrom;
        use std::i16;

        pass.raw.set_viewports(
            0,
            &[hal::pso::Viewport {
                rect: hal::pso::Rect {
                    x: i16::try_from(x.round() as i64).unwrap_or(0),
                    y: i16::try_from(y.round() as i64).unwrap_or(0),
                    w: i16::try_from(w.round() as i64).unwrap_or(i16::MAX),
                    h: i16::try_from(h.round() as i64).unwrap_or(i16::MAX),
                },
                depth: min_depth .. max_depth,
            }],
        );
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_viewport(
    pass_id: RenderPassId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    min_depth: f32,
    max_depth: f32,
) {
    gfx_select!(pass_id => render_pass_set_viewport(pass_id, x, y, w, h, min_depth, max_depth))
}

pub fn render_pass_set_scissor_rect<B: GfxBackend>(
    pass_id: RenderPassId,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.render_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    unsafe {
        use std::convert::TryFrom;
        use std::i16;

        pass.raw.set_scissors(
            0,
            &[hal::pso::Rect {
                x: i16::try_from(x).unwrap_or(0),
                y: i16::try_from(y).unwrap_or(0),
                w: i16::try_from(w).unwrap_or(i16::MAX),
                h: i16::try_from(h).unwrap_or(i16::MAX),
            }],
        );
    }
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_set_scissor_rect(
    pass_id: RenderPassId,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    gfx_select!(pass_id => render_pass_set_scissor_rect(pass_id, x, y, w, h))
}

#[no_mangle]
pub extern "C" fn wgpu_render_pass_execute_bundles(
    _pass_id: RenderPassId,
    _bundles: *const RenderBundleId,
    _bundles_length: usize,
) {
    unimplemented!()
}
