use crate::{
    command::bind::{Binder, LayoutChange},
    device::all_buffer_stages,
    gfx_select,
    hub::{GfxBackend, Token},
    track::{Stitch, TrackerSet},
    BindGroupId,
    BufferAddress,
    BufferId,
    BufferUsage,
    CommandBuffer,
    CommandBufferId,
    ComputePassId,
    ComputePipelineId,
    RawString,
    Stored,
    BIND_BUFFER_ALIGNMENT,
};

use hal::{self, command::RawCommandBuffer};
use log::trace;

use std::{iter, slice};

#[derive(Debug)]
pub struct ComputePass<B: hal::Backend> {
    raw: B::CommandBuffer,
    cmb_id: Stored<CommandBufferId>,
    binder: Binder,
    trackers: TrackerSet,
}

impl<B: hal::Backend> ComputePass<B> {
    pub(crate) fn new(
        raw: B::CommandBuffer,
        cmb_id: Stored<CommandBufferId>,
        trackers: TrackerSet,
    ) -> Self {
        ComputePass {
            raw,
            cmb_id,
            binder: Binder::default(),
            trackers,
        }
    }
}

// Common routines between render/compute

pub fn compute_pass_end_pass<B: GfxBackend>(pass_id: ComputePassId) {
    let mut token = Token::root();
    let hub = B::hub();
    let (mut cmb_guard, mut token) = hub.command_buffers.write(&mut token);
    let (pass, _) = hub.compute_passes.unregister(pass_id, &mut token);
    let cmb = &mut cmb_guard[pass.cmb_id.value];

    // There are no transitions to be made: we've already been inserting barriers
    // into the parent command buffer while recording this compute pass.
    cmb.trackers = pass.trackers;
    cmb.raw.push(pass.raw);
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_end_pass(pass_id: ComputePassId) {
    gfx_select!(pass_id => compute_pass_end_pass(pass_id))
}

pub fn compute_pass_set_bind_group<B: GfxBackend>(
    pass_id: ComputePassId,
    index: u32,
    bind_group_id: BindGroupId,
    offsets: &[BufferAddress],
) {
    let hub = B::hub();
    let mut token = Token::root();

    let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
    let (mut pass_guard, mut token) = hub.compute_passes.write(&mut token);
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

    //Note: currently, WebGPU compute passes have synchronization defined
    // at a dispatch granularity, so we insert the necessary barriers here.
    let (buffer_guard, mut token) = hub.buffers.read(&mut token);
    let (texture_guard, _) = hub.textures.read(&mut token);

    trace!(
        "Encoding barriers on binding of {:?} in pass {:?}",
        bind_group_id,
        pass_id
    );
    CommandBuffer::insert_barriers(
        &mut pass.raw,
        &mut pass.trackers,
        &bind_group.used,
        Stitch::Last,
        &*buffer_guard,
        &*texture_guard,
    );

    if let Some((pipeline_layout_id, follow_up_sets, follow_up_offsets)) = pass
        .binder
        .provide_entry(index as usize, bind_group_id, bind_group, offsets)
    {
        let bind_groups = iter::once(bind_group.raw.raw())
            .chain(follow_up_sets.map(|bg_id| bind_group_guard[bg_id].raw.raw()));
        unsafe {
            pass.raw.bind_compute_descriptor_sets(
                &pipeline_layout_guard[pipeline_layout_id].raw,
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
pub extern "C" fn wgpu_compute_pass_set_bind_group(
    pass_id: ComputePassId,
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
    gfx_select!(pass_id => compute_pass_set_bind_group(pass_id, index, bind_group_id, offsets))
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_push_debug_group(_pass_id: ComputePassId, _label: RawString) {
    //TODO
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_pop_debug_group(_pass_id: ComputePassId) {
    //TODO
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_insert_debug_marker(
    _pass_id: ComputePassId,
    _label: RawString,
) {
    //TODO
}

// Compute-specific routines

pub fn compute_pass_dispatch<B: GfxBackend>(pass_id: ComputePassId, x: u32, y: u32, z: u32) {
    let hub = B::hub();
    let mut token = Token::root();
    let (mut pass_guard, _) = hub.compute_passes.write(&mut token);
    unsafe {
        pass_guard[pass_id].raw.dispatch([x, y, z]);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_dispatch(pass_id: ComputePassId, x: u32, y: u32, z: u32) {
    gfx_select!(pass_id => compute_pass_dispatch(pass_id, x, y, z))
}

pub fn compute_pass_dispatch_indirect<B: GfxBackend>(
    pass_id: ComputePassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (buffer_guard, _) = hub.buffers.read(&mut token);
    let (mut pass_guard, _) = hub.compute_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];

    let (src_buffer, src_pending) = pass.trackers.buffers.use_replace(
        &*buffer_guard,
        indirect_buffer_id,
        (),
        BufferUsage::INDIRECT,
    );

    let barriers = src_pending.map(|pending| hal::memory::Barrier::Buffer {
        states: pending.to_states(),
        target: &src_buffer.raw,
        families: None,
        range: None .. None,
    });

    unsafe {
        pass.raw.pipeline_barrier(
            all_buffer_stages() .. all_buffer_stages(),
            hal::memory::Dependencies::empty(),
            barriers,
        );
        pass.raw.dispatch_indirect(&src_buffer.raw, indirect_offset);
    }
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_dispatch_indirect(
    pass_id: ComputePassId,
    indirect_buffer_id: BufferId,
    indirect_offset: BufferAddress,
) {
    gfx_select!(pass_id => compute_pass_dispatch_indirect(pass_id, indirect_buffer_id, indirect_offset))
}

pub fn compute_pass_set_pipeline<B: GfxBackend>(
    pass_id: ComputePassId,
    pipeline_id: ComputePipelineId,
) {
    let hub = B::hub();
    let mut token = Token::root();
    let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
    let (mut pass_guard, mut token) = hub.compute_passes.write(&mut token);
    let pass = &mut pass_guard[pass_id];
    let (pipeline_guard, _) = hub.compute_pipelines.read(&mut token);
    let pipeline = &pipeline_guard[pipeline_id];

    unsafe {
        pass.raw.bind_compute_pipeline(&pipeline.raw);
    }

    // Rebind resources
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
                        pass.raw.bind_compute_descriptor_sets(
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
}

#[no_mangle]
pub extern "C" fn wgpu_compute_pass_set_pipeline(
    pass_id: ComputePassId,
    pipeline_id: ComputePipelineId,
) {
    gfx_select!(pass_id => compute_pass_set_pipeline(pass_id, pipeline_id))
}
