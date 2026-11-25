//! Query pools: the counters a console title reads back.
//!
//! A console title asks for occlusion results and pipeline statistics
//! through a query heap and reads them into a buffer it owns. Vulkan's
//! model matches (a `VkQueryPool` plus `vkCmdCopyQueryPoolResults`), so
//! what is left to decide is the *translation* of the console's query types
//! and of the result layout the title expects — both pure functions, so
//! they are testable without a driver.
//!
//! The layout matters because the title reads the buffer itself: the
//! console writes one 64-bit word per occlusion query and a packed struct
//! per statistics query, so this module computes exactly those sizes rather
//! than letting Vulkan's 32-bit occlusion words reach the title.

use ash::vk;

/// A console query type, by what it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// How many of the samples passed the depth test.
    Occlusion,
    /// How many samples were counted for a binary occlusion query.
    BinaryOcclusion,
    /// How many primitives reached the pipeline, how many were clipped,
    /// and so on.
    PipelineStatistics,
    /// The value the GPU clock had, in ticks.
    Timestamp,
    /// One of the title's own counters, which this backend makes no
    /// promises about.
    Custom,
}

/// Every statistic a core query pool can count.
///
/// Spelled out because `ash` has no `ALL`. The task and mesh shader
/// counters are extension-only, and a core query pool rejects them, so they
/// are deliberately absent: a caller that enables those extensions adds
/// them itself.
pub fn every_statistic() -> vk::QueryPipelineStatisticFlags {
    use vk::QueryPipelineStatisticFlags as F;
    F::INPUT_ASSEMBLY_VERTICES
        | F::INPUT_ASSEMBLY_PRIMITIVES
        | F::VERTEX_SHADER_INVOCATIONS
        | F::GEOMETRY_SHADER_INVOCATIONS
        | F::GEOMETRY_SHADER_PRIMITIVES
        | F::CLIPPING_INVOCATIONS
        | F::CLIPPING_PRIMITIVES
        | F::FRAGMENT_SHADER_INVOCATIONS
        | F::TESSELLATION_CONTROL_SHADER_PATCHES
        | F::TESSELLATION_EVALUATION_SHADER_INVOCATIONS
        | F::COMPUTE_SHADER_INVOCATIONS
}

impl Kind {
    /// The Vulkan query type this is.
    ///
    /// A binary occlusion query has no Vulkan equivalent that yields a
    /// count, so it becomes a plain occlusion query: the count is then zero
    /// or the sample count, which is all a binary query reports anyway.
    pub fn vulkan(self) -> vk::QueryType {
        match self {
            Kind::Occlusion | Kind::BinaryOcclusion => vk::QueryType::OCCLUSION,
            Kind::PipelineStatistics => vk::QueryType::PIPELINE_STATISTICS,
            Kind::Timestamp => vk::QueryType::TIMESTAMP,
            Kind::Custom => vk::QueryType::OCCLUSION,
        }
    }

    /// The statistics this query counts, empty for every non-statistics
    /// kind (which Vulkan requires).
    pub fn statistics(self) -> vk::QueryPipelineStatisticFlags {
        if self == Kind::PipelineStatistics {
            every_statistic()
        } else {
            vk::QueryPipelineStatisticFlags::empty()
        }
    }

    /// How many `u32` words one result of this kind occupies in the buffer
    /// the title reads.
    ///
    /// A console occlusion result is 64-bit (a GPU counts more samples than
    /// 32 bits hold, and the title reads a `u64`), so it is two words even
    /// though Vulkan would hand back one.
    pub fn words(self) -> u32 {
        match self {
            Kind::Occlusion | Kind::BinaryOcclusion => 2,
            Kind::Timestamp => 2,
            Kind::PipelineStatistics => 7,
            Kind::Custom => 2,
        }
    }

    /// Whether Vulkan hands the result back as 64-bit words.
    pub fn wide(self) -> bool {
        matches!(
            self,
            Kind::Occlusion | Kind::BinaryOcclusion | Kind::Timestamp
        )
    }
}

/// The stride between consecutive results, in bytes.
pub fn stride(kind: Kind) -> u64 {
    u64::from(kind.words()) * 4
}

/// The offset of query `index`'s result in a buffer of `kind` results, or
/// `None` when even the offset alone would not fit in a `u64`.
pub fn offset(kind: Kind, index: u32) -> Option<u64> {
    u64::from(index).checked_mul(stride(kind))
}

/// The size a buffer needs for `count` results of `kind`, saturating at
/// `u64::MAX` because `count` comes from a title and need not be sane.
pub fn needed(kind: Kind, count: u32) -> u64 {
    u64::from(count).saturating_mul(stride(kind))
}

/// The `vkQueryPoolCreateInfo` for `count` queries of `kind`.
pub fn create_info(kind: Kind, count: u32) -> vk::QueryPoolCreateInfo<'static> {
    vk::QueryPoolCreateInfo::default()
        .query_type(kind.vulkan())
        .query_count(count)
        .pipeline_statistics(kind.statistics())
}

/// Whether a query is ready to be read.
///
/// `vkGetQueryPoolResults` answers `NOT_READY` before the GPU has written
/// the result, which is not a failure: a caller polls, so this says which
/// answer means "ask again".
pub fn ready(result: vk::Result) -> Result<bool, String> {
    match result {
        vk::Result::SUCCESS => Ok(true),
        vk::Result::NOT_READY => Ok(false),
        other => Err(format!("vkGetQueryPoolResults: {other:?}")),
    }
}
