//! The query translation policy. Pure, so no driver.

use ash::vk;

use crate::query::*;

const EVERY_KIND: [Kind; 5] = [
    Kind::Occlusion,
    Kind::BinaryOcclusion,
    Kind::PipelineStatistics,
    Kind::Timestamp,
    Kind::Custom,
];

#[test]
fn occlusion_and_binary_occlusion_both_become_occlusion() {
    assert_eq!(Kind::Occlusion.vulkan(), vk::QueryType::OCCLUSION);
    assert_eq!(Kind::BinaryOcclusion.vulkan(), vk::QueryType::OCCLUSION);
}

#[test]
fn every_kind_maps_to_a_vulkan_type() {
    assert_eq!(
        Kind::PipelineStatistics.vulkan(),
        vk::QueryType::PIPELINE_STATISTICS
    );
    assert_eq!(Kind::Timestamp.vulkan(), vk::QueryType::TIMESTAMP);
    assert_eq!(Kind::Custom.vulkan(), vk::QueryType::OCCLUSION);
}

#[test]
fn only_statistics_asks_for_statistic_flags() {
    for kind in EVERY_KIND {
        if kind == Kind::PipelineStatistics {
            assert_eq!(kind.statistics(), every_statistic());
            assert!(!kind.statistics().is_empty());
        } else {
            assert!(kind.statistics().is_empty(), "{kind:?}");
        }
    }
}

#[test]
fn the_extension_only_counters_are_not_asked_for() {
    let flags = every_statistic();
    assert!(!flags.contains(vk::QueryPipelineStatisticFlags::TASK_SHADER_INVOCATIONS_EXT));
    assert!(!flags.contains(vk::QueryPipelineStatisticFlags::MESH_SHADER_INVOCATIONS_EXT));
}

#[test]
fn a_console_occlusion_result_is_64_bit_even_though_vulkan_is_32() {
    assert_eq!(Kind::Occlusion.words(), 2);
    assert!(Kind::Occlusion.wide());
    assert_eq!(stride(Kind::Occlusion), 8);
}

#[test]
fn statistics_results_are_a_packed_struct() {
    assert_eq!(Kind::PipelineStatistics.words(), 7);
    assert_eq!(stride(Kind::PipelineStatistics), 28);
    assert!(!Kind::PipelineStatistics.wide());
}

#[test]
fn offsets_are_consecutive_and_start_at_zero() {
    assert_eq!(offset(Kind::Occlusion, 0), Some(0));
    assert_eq!(offset(Kind::Occlusion, 1), Some(8));
    assert_eq!(offset(Kind::Occlusion, 10), Some(80));
    assert_eq!(offset(Kind::Timestamp, 3), Some(24));
    assert_eq!(offset(Kind::PipelineStatistics, 2), Some(56));
}

#[test]
fn a_buffer_size_is_the_stride_times_the_count() {
    assert_eq!(needed(Kind::Occlusion, 0), 0);
    assert_eq!(needed(Kind::Occlusion, 1), 8);
    assert_eq!(needed(Kind::Occlusion, 100), 800);
    assert_eq!(needed(Kind::PipelineStatistics, 100), 2800);
}

#[test]
fn an_absurd_query_count_saturates() {
    assert_eq!(needed(Kind::Occlusion, u32::MAX), u64::from(u32::MAX) * 8);
    assert_eq!(needed(Kind::PipelineStatistics, u32::MAX), u64::from(u32::MAX) * 28);
}

#[test]
fn the_create_info_carries_the_type_the_count_and_the_flags() {
    let info = create_info(Kind::PipelineStatistics, 32);
    assert_eq!(info.query_type, vk::QueryType::PIPELINE_STATISTICS);
    assert_eq!(info.query_count, 32);
    assert_eq!(info.pipeline_statistics, every_statistic());

    for kind in [
        Kind::Occlusion,
        Kind::BinaryOcclusion,
        Kind::Timestamp,
        Kind::Custom,
    ] {
        let info = create_info(kind, 4);
        assert_eq!(info.query_count, 4);
        assert!(info.pipeline_statistics.is_empty(), "{kind:?}");
    }
}

#[test]
fn a_zero_count_is_passed_through() {
    assert_eq!(create_info(Kind::Occlusion, 0).query_count, 0);
}

#[test]
fn not_ready_means_ask_again_not_failure() {
    assert_eq!(ready(vk::Result::SUCCESS), Ok(true));
    assert_eq!(ready(vk::Result::NOT_READY), Ok(false));
}

#[test]
fn a_real_failure_says_what_it_was() {
    let error = ready(vk::Result::ERROR_DEVICE_LOST).unwrap_err();
    assert!(error.contains("DEVICE_LOST"), "{error}");
}
