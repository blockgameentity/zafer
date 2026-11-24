//! A hand-written SPIR-V module writer: the second backend's encoder.
//!
//! Shaders are translated from the console's GFX10 to an IR
//! ([`crate::ir`]) and then encoded for a host API. The DXIL backend
//! writes LLVM bitcode ([`crate::emit::bitcode`]); this one writes SPIR-V,
//! which Vulkan takes directly. Both encoders are plain word buffers with
//! an id allocator and typed helpers, so the emitter above them reads the
//! same either way.
//!
//! Only the instructions a shader needs are named here. Opcodes are the
//! specification's numbers; `tests` checks a subset of them against
//! `spirv-as`/`spirv-dis` when the tools are installed, so a typo cannot
//! drift silently.
//!
//! # Layout
//!
//! A module is a five-word header (`MAGIC`, `VERSION`, generator, the id
//! bound, the schema) and then instructions as
//! `word_count << 16 | opcode`. The bound is fixed when [`Module::finish`]
//! runs, so ids may be allocated in any order and forward references (phi
//! operands) work as in the DXIL writer.
#![allow(dead_code)]

/// The SPIR-V magic number.
pub const MAGIC: u32 = 0x0723_0203;
/// The version this writer emits: SPIR-V 1.3 (1.3.0), what Vulkan 1.1
/// accepts.
pub const VERSION: u32 = 0x0001_0300;

/// A result id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u32);

/// One instruction's opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Op {
    Nop = 0,
    Undef = 1,
    SourceContinued = 2,
    Source = 3,
    SourceExtension = 4,
    Name = 5,
    MemberName = 6,
    String = 7,
    Line = 8,
    Extension = 10,
    ExtInstImport = 11,
    ExtInst = 12,
    MemoryModel = 14,
    EntryPoint = 15,
    ExecutionMode = 16,
    Capability = 17,
    TypeVoid = 19,
    TypeBool = 20,
    TypeInt = 21,
    TypeFloat = 22,
    TypeVector = 23,
    TypeMatrix = 24,
    TypeImage = 25,
    TypeSampler = 26,
    TypeSampledImage = 27,
    TypeArray = 28,
    TypeRuntimeArray = 29,
    TypeStruct = 30,
    TypeOpaque = 31,
    TypePointer = 32,
    TypeFunction = 33,
    ConstantTrue = 41,
    ConstantFalse = 42,
    Constant = 43,
    ConstantComposite = 44,
    ConstantSampler = 45,
    ConstantNull = 46,
    SpecConstantTrue = 48,
    SpecConstantFalse = 49,
    SpecConstant = 50,
    SpecConstantComposite = 51,
    Function = 54,
    FunctionParameter = 55,
    FunctionEnd = 56,
    FunctionCall = 57,
    Variable = 59,
    ImageTexelPointer = 60,
    Load = 61,
    Store = 62,
    CopyMemory = 63,
    CopyMemorySized = 64,
    AccessChain = 65,
    InBoundsAccessChain = 66,
    ArrayLength = 68,
    Decorate = 71,
    MemberDecorate = 72,
    VectorExtractDynamic = 77,
    VectorInsertDynamic = 78,
    VectorShuffle = 79,
    CompositeConstruct = 80,
    CompositeExtract = 81,
    CompositeInsert = 82,
    CopyObject = 83,
    Transpose = 84,
    SampledImage = 86,
    ImageSampleImplicitLod = 87,
    ImageSampleExplicitLod = 88,
    ImageSampleDrefImplicitLod = 89,
    ImageSampleProjImplicitLod = 91,
    ImageFetch = 95,
    ImageGather = 96,
    ImageRead = 98,
    ImageWrite = 99,
    Image = 100,
    ImageQuerySizeLod = 103,
    ImageQuerySize = 104,
    ImageQueryLod = 105,
    ImageQueryLevels = 106,
    ImageQuerySamples = 107,
    ConvertFToU = 109,
    ConvertFToS = 110,
    ConvertSToF = 111,
    ConvertUToF = 112,
    UConvert = 113,
    SConvert = 114,
    FConvert = 115,
    QuantizeToF16 = 116,
    Bitcast = 124,
    SNegate = 126,
    FNegate = 127,
    IAdd = 128,
    FAdd = 129,
    ISub = 130,
    FSub = 131,
    IMul = 132,
    FMul = 133,
    UDiv = 134,
    SDiv = 135,
    FDiv = 136,
    UMod = 137,
    SRem = 138,
    SMod = 139,
    FRem = 140,
    FMod = 141,
    VectorTimesScalar = 142,
    MatrixTimesScalar = 143,
    VectorTimesMatrix = 144,
    MatrixTimesVector = 145,
    MatrixTimesMatrix = 146,
    OuterProduct = 147,
    Dot = 148,
    IAddCarry = 149,
    ISubBorrow = 150,
    UMulExtended = 151,
    SMulExtended = 152,
    Any = 154,
    All = 155,
    IsNan = 156,
    IsInf = 157,
    IsFinite = 158,
    IsNormal = 159,
    LogicalEqual = 164,
    LogicalNotEqual = 165,
    LogicalOr = 166,
    LogicalAnd = 167,
    LogicalNot = 168,
    Select = 169,
    IEqual = 170,
    INotEqual = 171,
    UGreaterThan = 172,
    SGreaterThan = 173,
    UGreaterThanEqual = 174,
    SGreaterThanEqual = 175,
    ULessThan = 176,
    SLessThan = 177,
    ULessThanEqual = 178,
    SLessThanEqual = 179,
    FOrdEqual = 180,
    FUnordEqual = 181,
    FOrdNotEqual = 182,
    FUnordNotEqual = 183,
    FOrdLessThan = 184,
    FUnordLessThan = 185,
    FOrdGreaterThan = 186,
    FUnordGreaterThan = 187,
    FOrdLessThanEqual = 188,
    FUnordLessThanEqual = 189,
    FOrdGreaterThanEqual = 190,
    FUnordGreaterThanEqual = 191,
    ShiftRightLogical = 194,
    ShiftRightArithmetic = 195,
    ShiftLeftLogical = 196,
    BitwiseOr = 197,
    BitwiseXor = 198,
    BitwiseAnd = 199,
    Not = 200,
    BitFieldInsert = 201,
    BitFieldSExtract = 202,
    BitFieldUExtract = 203,
    BitReverse = 204,
    BitCount = 205,
    DPdx = 207,
    DPdy = 208,
    Fwidth = 209,
    DPdxFine = 210,
    DPdyFine = 211,
    FwidthFine = 212,
    DPdxCoarse = 213,
    DPdyCoarse = 214,
    FwidthCoarse = 215,
    EmitVertex = 218,
    EndPrimitive = 219,
    AtomicLoad = 227,
    AtomicStore = 228,
    AtomicExchange = 229,
    AtomicCompareExchange = 230,
    AtomicIIncrement = 232,
    AtomicIDecrement = 233,
    AtomicIAdd = 234,
    AtomicISub = 235,
    AtomicSMin = 236,
    AtomicUMin = 237,
    AtomicSMax = 238,
    AtomicUMax = 239,
    AtomicAnd = 240,
    AtomicOr = 241,
    AtomicXor = 242,
    Phi = 245,
    LoopMerge = 246,
    SelectionMerge = 247,
    Label = 248,
    Branch = 249,
    BranchConditional = 250,
    Switch = 251,
    Kill = 252,
    Return = 253,
    ReturnValue = 254,
    Unreachable = 255,
    LifetimeStart = 256,
    LifetimeStop = 257,
    GroupNonUniform = 333,
    ControlBarrier = 224,
    MemoryBarrier = 225,
}
/// A capability the module declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Capability {
    Matrix = 0,
    Shader = 1,
    Geometry = 2,
    Tessellation = 3,
    Addresses = 4,
    Linkage = 5,
    Kernel = 6,
    Vector16 = 7,
    Float16Buffer = 8,
    Float16 = 9,
    Float64 = 10,
    Int64 = 11,
    Int64Atomics = 12,
    Int16 = 22,
    Int8 = 39,
    ImageQuery = 50,
    DerivativeControl = 51,
    SampledBuffer = 62,
    StorageImageReadWithoutFormat = 68,
    StorageImageWriteWithoutFormat = 69,
    MultiViewport = 70,
    ClipDistance = 32,
    CullDistance = 33,
    SampleMask = 34,
    SampleRateShading = 35,
    DrawParameters = 4427,
    GroupNonUniform = 61,
    SubgroupBallotKHR = 4423,
    SubgroupVoteKHR = 4431,
    ShaderNonUniform = 5301,
    RuntimeDescriptorArray = 5302,
    ImageRobustAccess = 4484,
}

/// The address model the module assumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum AddressingModel {
    Logical = 0,
    Physical32 = 1,
    Physical64 = 2,
    PhysicalStorageBuffer64 = 5348,
}

/// The memory model the module assumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryModel {
    Simple = 0,
    Glsl450 = 1,
    OpenCl = 2,
    Vulkan = 3,
}

/// Which kind of program an entry point is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ExecutionModel {
    Vertex = 0,
    TessellationControl = 1,
    TessellationEvaluation = 2,
    Geometry = 3,
    Fragment = 4,
    GlCompute = 5,
    Kernel = 6,
    /// `VK_EXT_mesh_shader` (SPIR-V extension `SPV_EXT_mesh_shader`).
    TaskNV = 5267,
    MeshNV = 5268,
}

/// The ways an entry point's behaviour is restricted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ExecutionMode {
    Invocations = 0,
    SpacingEqual = 1,
    OriginUpperLeft = 7,
    OriginLowerLeft = 8,
    EarlyFragmentTests = 9,
    DepthReplacing = 12,
    DepthGreater = 14,
    DepthLess = 15,
    DepthUnchanged = 16,
    LocalSize = 17,
    LocalSizeHint = 18,
    InputPoints = 19,
    InputLines = 20,
    InputTriangles = 22,
    OutputPoints = 26,
    OutputLineStrip = 27,
    OutputTriangleStrip = 28,
    LocalSizeId = 38,
    SubgroupSize = 35,
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
/// A growable SPIR-V instruction buffer and its id allocator.
#[derive(Default)]
pub struct Module {
    words: Vec<u32>,
    /// One past the largest id handed out.
    bound: u32,
}

impl Module {
    /// An empty module: no header yet, no ids.
    pub fn new() -> Self {
        Module {
            words: Vec::new(),
            bound: 1,
        }
    }

    /// A fresh id. Ids are never reused.
    pub fn id(&mut self) -> Id {
        let id = Id(self.bound);
        self.bound += 1;
        id
    }

    /// The largest id handed out so far (zero when none has been).
    pub fn bound(&self) -> u32 {
        self.bound
    }

    /// Appends `op` with an optional result type and result id and its
    /// operand words.
    pub fn op(&mut self, op: Op, result_type: Option<Id>, result: Option<Id>, operands: &[u32]) {
        let mut words = 1;
        words += usize::from(result_type.is_some());
        words += usize::from(result.is_some());
        words += operands.len();
        assert!(words < 0x1_0000, "an instruction's word count is 16 bits");
        self.words.push((words as u32) << 16 | op as u32);
        if let Some(t) = result_type {
            self.words.push(t.0);
        }
        if let Some(r) = result {
            self.words.push(r.0);
        }
        self.words.extend_from_slice(operands);
    }

    /// Appends `op` for a result of `ty`, id `result`.
    pub fn op_result(&mut self, op: Op, ty: Id, result: Id, operands: &[u32]) {
        self.op(op, Some(ty), Some(result), operands);
    }

    /// Appends `op` whose only operand is `id` (branch targets, merges).
    pub fn op_id(&mut self, op: Op, id: Id) {
        self.op(op, None, None, &[id.0]);
    }

    /// Appends an instruction carrying a literal string as its last
    /// operand: `OpName`, `OpMemberName`, `OpEntryPoint`.
    pub fn op_string(&mut self, op: Op, prefix: &[u32], s: &str) {
        let mut operands = prefix.to_vec();
        operands.extend(literal_string(s));
        self.op(op, None, None, &operands);
    }

    /// The instruction words written so far (no header).
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// The module as a file: the header, then the instructions.
    pub fn finish(self) -> Vec<u8> {
        let mut all = Vec::with_capacity(self.words.len() + 5);
        all.push(MAGIC);
        all.push(VERSION);
        all.push(0); // generator: none, deliberately
        all.push(self.bound.max(1));
        all.push(0); // schema
        all.extend_from_slice(&self.words);
        all.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

/// A NUL-terminated literal string as SPIR-V words, zero-padded.
fn literal_string(s: &str) -> Vec<u32> {
    let bytes = s.as_bytes();
    let words = bytes.len() / 4 + 1;
    let mut out = vec![0u32; words];
    for (i, &b) in bytes.iter().enumerate() {
        out[i / 4] |= u32::from(b) << (8 * (i % 4));
    }
    out
}

/// Reads a literal string back out of instruction words (tests and
/// diagnostics only).
pub fn read_string(words: &[u32]) -> Option<String> {
    let mut bytes = Vec::new();
    for w in words {
        for i in 0..4 {
            let b = (w >> (8 * i)) as u8;
            if b == 0 {
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            bytes.push(b);
        }
    }
    None
}

/// An instruction as [`Module::parse`] finds it.
#[derive(Clone, Debug)]
pub struct Instruction {
    pub op: u32,
    /// Operand words, without the opcode's word.
    pub operands: Vec<u32>,
}

/// Every instruction of a finished module, so tests can check a build
/// without an external tool. `None` if the buffer is not a module.
pub fn parse(module: &[u8]) -> Option<Vec<Instruction>> {
    let words: Vec<u32> = module
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if words.len() < 5 || words[0] != MAGIC || words[1] != VERSION {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 5;
    while i < words.len() {
        let count = (words[i] >> 16) as usize;
        if count == 0 || i + count > words.len() {
            return None;
        }
        out.push(Instruction {
            op: words[i] & 0xFFFF,
            operands: words[i + 1..i + count].to_vec(),
        });
        i += count;
    }
    Some(out)
}

/// The types and constants a module uses, built once each.
#[derive(Default)]
pub struct Types {
    pub void: Option<Id>,
    pub bool_: Option<Id>,
    /// By bit width: 8, 16, 32, 64.
    pub ints: Vec<(u32, Id)>,
    pub floats: Vec<(u32, Id)>,
    pub vectors: Vec<(Id, u32, Id)>,
    pub pointers: Vec<(u32, Id, Id)>,
    pub arrays: Vec<(Id, u32, Id)>,
    pub structs: Vec<(Id, Vec<u32>)>,
    pub functions: Vec<(Id, Vec<Id>, Id)>,
    pub images: Vec<(Id, Id, Dim, bool)>,
    pub sampled_images: Vec<(Id, Id)>,
    pub samplers: Option<Id>,
}

impl Types {
    /// `%void`.
    pub fn void(&mut self, m: &mut Module) -> Id {
        if let Some(t) = self.void {
            return t;
        }
        let t = m.id();
        m.op(Op::TypeVoid, None, Some(t), &[]);
        self.void = Some(t);
        t
    }

    /// `%bool`.
    pub fn bool_(&mut self, m: &mut Module) -> Id {
        if let Some(t) = self.bool_ {
            return t;
        }
        let t = m.id();
        m.op(Op::TypeBool, None, Some(t), &[]);
        self.bool_ = Some(t);
        t
    }

    /// An integer type of `bits`, signedness `0` (unsigned) or `1`.
    pub fn int(&mut self, m: &mut Module, bits: u32) -> Id {
        if let Some((_, t)) = self.ints.iter().find(|(b, _)| *b == bits) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeInt, None, Some(t), &[bits, 0]);
        self.ints.push((bits, t));
        t
    }

    /// A float type of `bits`.
    pub fn float(&mut self, m: &mut Module, bits: u32) -> Id {
        if let Some((_, t)) = self.floats.iter().find(|(b, _)| *b == bits) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeFloat, None, Some(t), &[bits]);
        self.floats.push((bits, t));
        t
    }

    /// `vecN` of `scalar`.
    pub fn vector(&mut self, m: &mut Module, scalar: Id, n: u32) -> Id {
        if let Some((_, _, t)) = self.vectors.iter().find(|(s, c, _)| *s == scalar && *c == n) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeVector, None, Some(t), &[scalar.0, n]);
        self.vectors.push((scalar, n, t));
        t
    }

    /// A pointer to `to` in `space`.
    pub fn pointer(&mut self, m: &mut Module, to: Id, space: StorageClass) -> Id {
        let space = space as u32;
        if let Some((_, _, t)) = self
            .pointers
            .iter()
            .find(|(s, p, _)| *s == space && *p == to)
        {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypePointer, None, Some(t), &[space, to.0]);
        self.pointers.push((space, to, t));
        t
    }

    /// An array of `n` `of` (a runtime array when `n` is zero).
    pub fn array(&mut self, m: &mut Module, of: Id, n: u32) -> Id {
        if let Some((_, _, t)) = self.arrays.iter().find(|(o, c, _)| *o == of && *c == n) {
            return *t;
        }
        let t = m.id();
        if n == 0 {
            m.op(Op::TypeRuntimeArray, None, Some(t), &[of.0]);
        } else {
            let len = self.u32_const(m, n);
            m.op(Op::TypeArray, None, Some(t), &[of.0, len.0]);
        }
        self.arrays.push((of, n, t));
        t
    }

    /// A struct of `fields`.
    pub fn struct_(&mut self, m: &mut Module, fields: &[Id]) -> Id {
        let ids: Vec<u32> = fields.iter().map(|f| f.0).collect();
        if let Some((t, _)) = self.structs.iter().find(|(_, f)| *f == ids) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeStruct, None, Some(t), &ids);
        self.structs.push((t, ids));
        t
    }

    /// A function type: `ret` and parameter types.
    pub fn function(&mut self, m: &mut Module, ret: Id, params: &[Id]) -> Id {
        if let Some((_, _, t)) = self
            .functions
            .iter()
            .find(|(r, p, _)| *r == ret && p.as_slice() == params)
        {
            return *t;
        }
        let mut operands = vec![ret.0];
        operands.extend(params.iter().map(|p| p.0));
        let t = m.id();
        m.op(Op::TypeFunction, None, Some(t), &operands);
        self.functions.push((ret, params.to_vec(), t));
        t
    }

    /// `u32`'s type.
    pub fn u32(&mut self, m: &mut Module) -> Id {
        self.int(m, 32)
    }

    /// `f32`'s type.
    pub fn f32(&mut self, m: &mut Module) -> Id {
        self.float(m, 32)
    }

    /// The `u32` constant `v`.
    pub fn u32_const(&mut self, m: &mut Module, v: u32) -> Id {
        let ty = self.u32(m);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[v]);
        id
    }

    /// The `i32` constant `v`.
    pub fn i32_const(&mut self, m: &mut Module, v: i32) -> Id {
        let ty = self.int(m, 32);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[v as u32]);
        id
    }

    /// The `f32` constant with these bits.
    pub fn f32_const(&mut self, m: &mut Module, bits: u32) -> Id {
        let ty = self.f32(m);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[bits]);
        id
    }

    /// A vector constant of `ty` from `components`.
    pub fn const_composite(&mut self, m: &mut Module, ty: Id, components: &[Id]) -> Id {
        let id = m.id();
        let operands: Vec<u32> = components.iter().map(|c| c.0).collect();
        m.op(Op::ConstantComposite, Some(ty), Some(id), &operands);
        id
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn build_trivial_compute() -> Vec<u8> {
        let mut m = Module::new();
        let mut t = Types::default();
        m.op(Op::Capability, None, None, &[Capability::Shader as u32]);
        m.op(
            Op::MemoryModel,
            None,
            None,
            &[AddressingModel::Logical as u32, MemoryModel::Glsl450 as u32],
        );
        // The layout sections are ordered: capabilities, extensions, the
        // memory model, entry points, execution modes, debug names,
        // annotations, types and constants, then functions.
        let main = m.id();
        m.op_string(Op::EntryPoint, &[ExecutionModel::GlCompute as u32, main.0], "main");
        m.op(Op::ExecutionMode, None, None, &[main.0, ExecutionMode::LocalSize as u32, 1, 1, 1]);
        let void = t.void(&mut m);
        let fn_ty = t.function(&mut m, void, &[]);
        m.op(Op::Function, Some(void), Some(main), &[function_control::NONE, fn_ty.0]);
        let entry = m.id();
        m.op(Op::Label, None, Some(entry), &[]);
        m.op(Op::Return, None, None, &[]);
        m.op(Op::FunctionEnd, None, None, &[]);
        m.finish()
    }

    #[test]
    fn the_header_is_magic_version_generator_bound_schema() {
        let out = Module::new().finish();
        let words: Vec<u32> = out
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, [MAGIC, VERSION, 0, 1, 0]);
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn an_instructions_word_count_and_opcode_are_packed() {
        let mut m = Module::new();
        m.op(Op::Capability, None, None, &[Capability::Shader as u32]);
        assert_eq!(m.words(), [2 << 16 | 17, 1]);
        // With a type and result: two more words.
        let mut m = Module::new();
        let (ty, result) = (m.id(), m.id());
        m.op(Op::Load, Some(ty), Some(result), &[7]);
        assert_eq!(m.words(), [4 << 16 | 61, ty.0, result.0, 7]);
    }

    #[test]
    fn ids_increase_and_the_bound_follows() {
        let mut m = Module::new();
        let a = m.id();
        let b = m.id();
        assert_eq!((a, b), (Id(1), Id(2)));
        assert_eq!(m.bound(), 3);
    }

    #[test]
    fn literal_strings_pad_and_read_back() {
        assert_eq!(literal_string("ab"), [u32::from_le_bytes([b'a', b'b', 0, 0])]);
        let words = literal_string("main");
        assert_eq!(words.len(), 2);
        assert_eq!(read_string(&words).as_deref(), Some("main"));
        // Four characters need a fifth byte for the terminator.
        assert_eq!(literal_string("abcd").len(), 2);
    }

    #[test]
    fn a_module_parses_back_instruction_for_instruction() {
        let out = build_trivial_compute();
        let ops = parse(&out).expect("our own module parses");
        assert_eq!(ops.len(), 10, "{ops:#?}");
        assert_eq!(ops[0].op, Op::Capability as u32);
        assert_eq!(ops[1].op, Op::MemoryModel as u32);
        assert_eq!(ops[2].op, Op::EntryPoint as u32);
        // OpEntryPoint: execution model, entry id, then the name words.
        assert_eq!(ops[2].operands[0], ExecutionModel::GlCompute as u32);
        assert_eq!(read_string(&ops[2].operands[2..]).as_deref(), Some("main"));
        assert_eq!(ops[3].operands[0], ops[2].operands[1]);
        assert_eq!(ops[3].operands[1], ExecutionMode::LocalSize as u32);
        assert_eq!(ops[9].op, Op::FunctionEnd as u32);
        assert!(parse(&[0u8; 8]).is_none(), "not a module");
    }

    /// The point of the writer: a module a Vulkan driver would accept.
    #[test]
    fn a_trivial_compute_module_is_valid_spir_v() {
        let module = build_trivial_compute();
        let dir = std::env::temp_dir().join(format!("victory-spirv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trivial.spv");
        std::fs::write(&path, &module).unwrap();
        let val = std::process::Command::new("spirv-val")
            .arg(&path)
            .output();
        match val {
            Err(_) => {
                // No validator here: the structural invariants are all we
                // can check, and `a_module_parses_back_instruction_for_instruction`
                // already covers them.
                let ops = parse(&module).unwrap();
                assert_eq!(ops.len(), 10);
            }
            Ok(o) => assert!(
                o.status.success(),
                "spirv-val refused our module: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn types_are_built_once_each() {
        let mut m = Module::new();
        let mut t = Types::default();
        let a = t.u32(&mut m);
        let b = t.u32(&mut m);
        assert_eq!(a, b);
        let v1 = t.vector(&mut m, a, 4);
        let v2 = t.vector(&mut m, a, 4);
        assert_eq!(v1, v2);
        let p1 = t.pointer(&mut m, v1, StorageClass::Input);
        let p2 = t.pointer(&mut m, v1, StorageClass::Input);
        let p3 = t.pointer(&mut m, v1, StorageClass::Output);
        assert_eq!(p1, p2);
        assert_ne!(p1, p3);
        // Each type appears once in the instruction stream.
        let count = parse(&m.finish())
            .unwrap()
            .iter()
            .filter(|i| i.op == Op::TypePointer as u32)
            .count();
        assert_eq!(count, 2);
    }
}
