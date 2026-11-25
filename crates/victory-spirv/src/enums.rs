//! How the API reads a value (decorations), the interface, images and the
//! bit masks an instruction's operands carry.

/// What a value means to the API (interface and layout rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Decoration {
    RelaxedPrecision = 0,
    SpecId = 1,
    Block = 2,
    BufferBlock = 3,
    RowMajor = 4,
    ColMajor = 5,
    ArrayStride = 6,
    MatrixStride = 7,
    GlslShared = 8,
    GlslPacked = 9,
    Cpacked = 10,
    BuiltIn = 11,
    NoPerspective = 13,
    Flat = 14,
    Patch = 15,
    Centroid = 16,
    Sample = 17,
    Invariant = 18,
    Restrict = 19,
    Aliased = 20,
    Volatile = 21,
    Constant = 22,
    Coherent = 23,
    NonWritable = 24,
    NonReadable = 25,
    Uniform = 26,
    SaturatedConversion = 28,
    Stream = 29,
    Location = 30,
    Component = 31,
    Index = 32,
    Binding = 33,
    DescriptorSet = 34,
    Offset = 35,
    XfbBuffer = 36,
    XfbStride = 37,
    NoContraction = 42,
    InputAttachmentIndex = 43,
    Alignment = 44,
}

/// A value the API supplies or the pipeline expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum BuiltIn {
    Position = 0,
    PointSize = 1,
    ClipDistance = 3,
    CullDistance = 4,
    VertexId = 5,
    InstanceId = 6,
    PrimitiveId = 7,
    InvocationId = 8,
    Layer = 9,
    ViewportIndex = 10,
    TessLevelOuter = 11,
    TessLevelInner = 12,
    TessCoord = 13,
    PatchVertices = 14,
    FragCoord = 15,
    PointCoord = 16,
    FrontFacing = 17,
    SampleId = 18,
    SamplePosition = 19,
    SampleMask = 20,
    FragDepth = 22,
    HelperInvocation = 23,
    NumWorkgroups = 24,
    WorkgroupSize = 25,
    WorkgroupId = 26,
    LocalInvocationId = 27,
    GlobalInvocationId = 28,
    LocalInvocationIndex = 29,
    VertexIndex = 42,
    InstanceIndex = 43,
    SubgroupSize = 36,
    SubgroupLocalInvocationId = 41,
    SubgroupEqMask = 4416,
    SubgroupGeMask = 4417,
    SubgroupGtMask = 4418,
    SubgroupLeMask = 4419,
    SubgroupLtMask = 4420,
    BaseVertex = 4424,
    BaseInstance = 4425,
    DrawIndex = 4426,
    FragStencilRefEXT = 5014,
}

/// Where a variable lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum StorageClass {
    UniformConstant = 0,
    Input = 1,
    Uniform = 2,
    Output = 3,
    Workgroup = 4,
    CrossWorkgroup = 5,
    Private = 6,
    Function = 7,
    Generic = 8,
    PushConstant = 9,
    AtomicCounter = 10,
    Image = 11,
    StorageBuffer = 12,
}

/// An image's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Dim {
    OneD = 0,
    TwoD = 1,
    ThreeD = 2,
    Cube = 3,
    Rect = 4,
    Buffer = 5,
    SubpassData = 6,
}

/// An image access's extra operands, as a bit mask.
pub mod image_operands {
    pub const NONE: u32 = 0;
    pub const BIAS: u32 = 0x1;
    pub const LOD: u32 = 0x2;
    pub const GRAD: u32 = 0x4;
    pub const CONST_OFFSET: u32 = 0x8;
    pub const OFFSET: u32 = 0x10;
    pub const CONST_OFFSETS: u32 = 0x20;
    pub const SAMPLE: u32 = 0x40;
    pub const MIN_LOD: u32 = 0x80;
}

/// A memory access's extra operands, as a bit mask.
pub mod memory_access {
    pub const NONE: u32 = 0;
    pub const VOLATILE: u32 = 0x1;
    pub const ALIGNED: u32 = 0x2;
    pub const NONTEMPORAL: u32 = 0x4;
    pub const MAKE_TEXEL_AVAILABLE: u32 = 0x8;
    pub const MAKE_TEXEL_VISIBLE: u32 = 0x10;
    pub const NON_PRIVATE_TEXEL: u32 = 0x20;
}

/// A function's properties, as a bit mask.
pub mod function_control {
    pub const NONE: u32 = 0;
    pub const INLINE: u32 = 0x1;
    pub const DONT_INLINE: u32 = 0x2;
    pub const PURE: u32 = 0x4;
    pub const CONST: u32 = 0x8;
}

/// How the value a selection produces is chosen.
pub mod selection_control {
    pub const NONE: u32 = 0;
    pub const FLATTEN: u32 = 0x1;
    pub const DONT_FLATTEN: u32 = 0x2;
}

/// A loop's properties, as a bit mask.
pub mod loop_control {
    pub const NONE: u32 = 0;
    pub const UNROLL: u32 = 0x1;
    pub const DONT_UNROLL: u32 = 0x2;
}
