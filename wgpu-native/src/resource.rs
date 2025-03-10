use crate::{
    swap_chain::{SwapChainLink, SwapImageEpoch},
    BufferAddress,
    BufferMapReadCallback,
    BufferMapWriteCallback,
    DeviceId,
    Extent3d,
    LifeGuard,
    RefCount,
    Stored,
    TextureId,
};

use bitflags::bitflags;
use hal;
use parking_lot::Mutex;
use rendy_memory::MemoryBlock;

use std::borrow::Borrow;

bitflags! {
    #[repr(transparent)]
    pub struct BufferUsage: u32 {
        const MAP_READ = 1;
        const MAP_WRITE = 2;
        const COPY_SRC = 4;
        const COPY_DST = 8;
        const INDEX = 16;
        const VERTEX = 32;
        const UNIFORM = 64;
        const STORAGE = 128;
        const STORAGE_READ = 256;
        const INDIRECT = 512;
        const NONE = 0;
        /// The combination of all read-only usages.
        const READ_ALL = Self::MAP_READ.bits | Self::COPY_SRC.bits |
            Self::INDEX.bits | Self::VERTEX.bits | Self::UNIFORM.bits |
            Self::STORAGE_READ.bits | Self::INDIRECT.bits;
        /// The combination of all write-only and read-write usages.
        const WRITE_ALL = Self::MAP_WRITE.bits | Self::COPY_DST.bits | Self::STORAGE.bits;
        /// The combination of all usages that the are guaranteed to be be ordered by the hardware.
        /// If a usage is not ordered, then even if it doesn't change between draw calls, there
        /// still need to be pipeline barriers inserted for synchronization.
        const ORDERED = Self::READ_ALL.bits;
    }
}

#[repr(C)]
#[derive(Clone, Debug)]
pub struct BufferDescriptor {
    pub size: BufferAddress,
    pub usage: BufferUsage,
}

#[repr(C)]
#[derive(Debug)]
pub enum BufferMapAsyncStatus {
    Success,
    Error,
    Unknown,
    ContextLost,
}

#[derive(Clone, Debug)]
pub enum BufferMapOperation {
    Read(std::ops::Range<u64>, BufferMapReadCallback, *mut u8),
    Write(std::ops::Range<u64>, BufferMapWriteCallback, *mut u8),
}

unsafe impl Send for BufferMapOperation {}
unsafe impl Sync for BufferMapOperation {}

impl BufferMapOperation {
    pub(crate) fn call_error(self) {
        match self {
            BufferMapOperation::Read(_, callback, userdata) => {
                log::error!("wgpu_buffer_map_read_async failed: buffer mapping is pending");
                callback(BufferMapAsyncStatus::Error, std::ptr::null_mut(), userdata);
            }
            BufferMapOperation::Write(_, callback, userdata) => {
                log::error!("wgpu_buffer_map_write_async failed: buffer mapping is pending");
                callback(BufferMapAsyncStatus::Error, std::ptr::null_mut(), userdata);
            }
        }
    }
}

#[derive(Debug)]
pub struct Buffer<B: hal::Backend> {
    pub(crate) raw: B::Buffer,
    pub(crate) device_id: Stored<DeviceId>,
    pub(crate) memory: MemoryBlock<B>,
    pub(crate) size: BufferAddress,
    pub(crate) mapped_write_ranges: Vec<std::ops::Range<u64>>,
    pub(crate) pending_map_operation: Option<BufferMapOperation>,
    pub(crate) life_guard: LifeGuard,
}

impl<B: hal::Backend> Borrow<RefCount> for Buffer<B> {
    fn borrow(&self) -> &RefCount {
        &self.life_guard.ref_count
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum TextureDimension {
    D1,
    D2,
    D3,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum TextureFormat {
    // Normal 8 bit formats
    R8Unorm = 0,
    R8Snorm = 1,
    R8Uint = 2,
    R8Sint = 3,

    // Normal 16 bit formats
    R16Unorm = 4,
    R16Snorm = 5,
    R16Uint = 6,
    R16Sint = 7,
    R16Float = 8,

    Rg8Unorm = 9,
    Rg8Snorm = 10,
    Rg8Uint = 11,
    Rg8Sint = 12,

    // Normal 32 bit formats
    R32Uint = 13,
    R32Sint = 14,
    R32Float = 15,
    Rg16Unorm = 16,
    Rg16Snorm = 17,
    Rg16Uint = 18,
    Rg16Sint = 19,
    Rg16Float = 20,
    Rgba8Unorm = 21,
    Rgba8UnormSrgb = 22,
    Rgba8Snorm = 23,
    Rgba8Uint = 24,
    Rgba8Sint = 25,
    Bgra8Unorm = 26,
    Bgra8UnormSrgb = 27,

    // Packed 32 bit formats
    Rgb10a2Unorm = 28,
    Rg11b10Float = 29,

    // Normal 64 bit formats
    Rg32Uint = 30,
    Rg32Sint = 31,
    Rg32Float = 32,
    Rgba16Unorm = 33,
    Rgba16Snorm = 34,
    Rgba16Uint = 35,
    Rgba16Sint = 36,
    Rgba16Float = 37,

    // Normal 128 bit formats
    Rgba32Uint = 38,
    Rgba32Sint = 39,
    Rgba32Float = 40,

    // Depth and stencil formats
    Depth32Float = 41,
    Depth24Plus = 42,
    Depth24PlusStencil8 = 43,
}

bitflags! {
    #[repr(transparent)]
    pub struct TextureUsage: u32 {
        const COPY_SRC = 1;
        const COPY_DST = 2;
        const SAMPLED = 4;
        const STORAGE = 8;
        const OUTPUT_ATTACHMENT = 16;
        const NONE = 0;
        /// The combination of all read-only usages.
        const READ_ALL = Self::COPY_SRC.bits | Self::SAMPLED.bits;
        /// The combination of all write-only and read-write usages.
        const WRITE_ALL = Self::COPY_DST.bits | Self::STORAGE.bits | Self::OUTPUT_ATTACHMENT.bits;
        /// The combination of all usages that the are guaranteed to be be ordered by the hardware.
        /// If a usage is not ordered, then even if it doesn't change between draw calls, there
        /// still need to be pipeline barriers inserted for synchronization.
        const ORDERED = Self::READ_ALL.bits | Self::OUTPUT_ATTACHMENT.bits;
        const UNINITIALIZED = 0xFFFF;
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct TextureDescriptor {
    pub size: Extent3d,
    pub array_layer_count: u32,
    pub mip_level_count: u32,
    pub sample_count: u32,
    pub dimension: TextureDimension,
    pub format: TextureFormat,
    pub usage: TextureUsage,
}

#[derive(Debug)]
pub(crate) enum TexturePlacement<B: hal::Backend> {
    #[cfg_attr(not(not(feature = "remote")), allow(unused))]
    SwapChain(SwapChainLink<Mutex<SwapImageEpoch>>),
    Memory(MemoryBlock<B>),
}

impl<B: hal::Backend> TexturePlacement<B> {
    pub fn as_swap_chain(&self) -> &SwapChainLink<Mutex<SwapImageEpoch>> {
        match *self {
            TexturePlacement::SwapChain(ref link) => link,
            TexturePlacement::Memory(_) => panic!("Expected swap chain link!"),
        }
    }
}

#[derive(Debug)]
pub struct Texture<B: hal::Backend> {
    pub(crate) raw: B::Image,
    pub(crate) device_id: Stored<DeviceId>,
    pub(crate) kind: hal::image::Kind,
    pub(crate) format: TextureFormat,
    pub(crate) full_range: hal::image::SubresourceRange,
    pub(crate) placement: TexturePlacement<B>,
    pub(crate) life_guard: LifeGuard,
}

impl<B: hal::Backend> Borrow<RefCount> for Texture<B> {
    fn borrow(&self) -> &RefCount {
        &self.life_guard.ref_count
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum TextureAspect {
    All,
    StencilOnly,
    DepthOnly,
}

impl Default for TextureAspect {
    fn default() -> Self {
        TextureAspect::All
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum TextureViewDimension {
    D1,
    D2,
    D2Array,
    Cube,
    CubeArray,
    D3,
}

#[repr(C)]
#[derive(Debug)]
pub struct TextureViewDescriptor {
    pub format: TextureFormat,
    pub dimension: TextureViewDimension,
    pub aspect: TextureAspect,
    pub base_mip_level: u32,
    pub level_count: u32,
    pub base_array_layer: u32,
    pub array_layer_count: u32,
}

#[derive(Debug)]
pub struct TextureView<B: hal::Backend> {
    pub(crate) raw: B::ImageView,
    pub(crate) texture_id: Stored<TextureId>,
    //TODO: store device_id for quick access?
    pub(crate) format: TextureFormat,
    pub(crate) extent: hal::image::Extent,
    pub(crate) samples: hal::image::NumSamples,
    pub(crate) range: hal::image::SubresourceRange,
    pub(crate) is_owned_by_swap_chain: bool,
    #[cfg_attr(not(not(feature = "remote")), allow(dead_code))]
    pub(crate) life_guard: LifeGuard,
}

impl<B: hal::Backend> Borrow<RefCount> for TextureView<B> {
    fn borrow(&self) -> &RefCount {
        &self.life_guard.ref_count
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum AddressMode {
    ClampToEdge = 0,
    Repeat = 1,
    MirrorRepeat = 2,
}

impl Default for AddressMode {
    fn default() -> Self {
        AddressMode::ClampToEdge
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum FilterMode {
    Nearest = 0,
    Linear = 1,
}

impl Default for FilterMode {
    fn default() -> Self {
        FilterMode::Nearest
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum CompareFunction {
    Never = 0,
    Less = 1,
    Equal = 2,
    LessEqual = 3,
    Greater = 4,
    NotEqual = 5,
    GreaterEqual = 6,
    Always = 7,
}

impl CompareFunction {
    pub fn is_trivial(&self) -> bool {
        match *self {
            CompareFunction::Never | CompareFunction::Always => true,
            _ => false,
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct SamplerDescriptor {
    pub address_mode_u: AddressMode,
    pub address_mode_v: AddressMode,
    pub address_mode_w: AddressMode,
    pub mag_filter: FilterMode,
    pub min_filter: FilterMode,
    pub mipmap_filter: FilterMode,
    pub lod_min_clamp: f32,
    pub lod_max_clamp: f32,
    pub compare_function: CompareFunction,
}

#[derive(Debug)]
pub struct Sampler<B: hal::Backend> {
    pub(crate) raw: B::Sampler,
}
