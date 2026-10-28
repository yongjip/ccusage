use ccusage_test_support::fs_fixture;
use serde_json::{Value, json};

use super::{
    CodexServiceTier, CodexSpeedPolicy, PricingMap,
    aggregate::load_groups_from_directory,
    cli::{AgentReportKind, CostMode, SharedArgs},
    report::report_from_groups,
};

#[test]
fn prices_documented_responses_cache_writes_by_ttl() {
    let fixture = fs_fixture!({
        "sessions/response.jsonl": json!({
            "type": "response.completed",
            "timestamp": "2026-10-01T00:00:00Z",
            "response": {
                "model": "gpt-5.6-terra",
                "usage": {
                    "input_tokens": 12_000,
                    "input_tokens_details": {
                        "cached_tokens": 4_000,
                        "cache_write_tokens": 7_000,
                        "cache_write_tokens_details": {
                            "ttl_30m_tokens": 3_000,
                            "ttl_12h_tokens": 4_000,
                        },
                    },
                    "output_tokens": 100,
                    "total_tokens": 12_100,
                },
            },
        }).to_string(),
    });
    let shared = SharedArgs {
        timezone: Some("UTC".to_string()),
        ..SharedArgs::default()
    };
    let groups =
        load_groups_from_directory(&fixture.path("sessions"), &shared, AgentReportKind::Daily)
            .unwrap();
    let pricing = test_pricing();
    let report = report_from_groups(
        &groups,
        AgentReportKind::Daily,
        &pricing,
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
        CostMode::Calculate,
    );

    // $1/M ordinary input, $0.10/M reads, $1.25/M 30m writes,
    // $2.50/M 12h writes, and $2/M output. Writes are part of input.
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.01535).abs() < 1e-12);
    assert_eq!(report["totals"]["inputTokens"], 1_000);
    assert_eq!(report["totals"]["cacheReadTokens"], 4_000);
    assert_eq!(report["totals"]["cacheCreationTokens"], 7_000);
    assert_eq!(report["totals"]["totalTokens"], 12_100);
}

fn test_pricing() -> PricingMap {
    let mut pricing = PricingMap::default();
    pricing.load_json(
        r#"{
            "gpt-5.6-terra": {
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002,
                "cache_read_input_token_cost": 0.0000001,
                "cache_creation_input_token_cost": 0.00000125
            }
        }"#,
    );
    pricing
}

fn usage_with_details(details: Value) -> Value {
    json!({
        "input_tokens": 12_000,
        "input_tokens_details": {
            "cached_tokens": 4_000,
            "cache_write_tokens": 7_000,
            "cache_write_tokens_details": details,
        },
        "output_tokens": 100,
        "total_tokens": 12_100,
    })
}

fn response(usage: Value, timestamp: &str) -> Value {
    json!({
        "type": "response.completed",
        "timestamp": timestamp,
        "response": { "model": "gpt-5.6-terra", "usage": usage },
    })
}

fn report_lines(lines: &[Value], pricing: &PricingMap, speed: CodexSpeedPolicy) -> Value {
    let fixture = fs_fixture!({
        "sessions/response.jsonl": lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"),
    });
    let groups = load_groups_from_directory(
        &fixture.path("sessions"),
        &SharedArgs {
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        },
        AgentReportKind::Daily,
    )
    .unwrap();
    report_from_groups(
        &groups,
        AgentReportKind::Daily,
        pricing,
        speed,
        CostMode::Calculate,
    )
}

fn standard_cost(lines: &[Value]) -> f64 {
    report_lines(
        lines,
        &test_pricing(),
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
    )["totals"]["costUSD"]
        .as_f64()
        .unwrap()
}

#[test]
fn prices_explicit_zero_lifetime_buckets() {
    for (details, expected) in [
        (
            json!({ "ttl_30m_tokens": 7_000, "ttl_12h_tokens": 0 }),
            0.01035,
        ),
        (
            json!({ "ttl_30m_tokens": 0, "ttl_12h_tokens": 7_000 }),
            0.0191,
        ),
    ] {
        let cost = standard_cost(&[response(
            usage_with_details(details),
            "2026-10-01T00:00:00Z",
        )]);
        assert!((cost - expected).abs() < 1e-12);
    }
}

#[test]
fn does_not_replace_explicit_flat_zero_counters_with_nested_usage() {
    let mut usage = usage_with_details(json!({ "ttl_30m_tokens": 3_000, "ttl_12h_tokens": 4_000 }));
    usage["cached_input_tokens"] = json!(0);
    usage["cache_write_input_tokens"] = json!(0);
    let cost = standard_cost(&[response(usage, "2026-10-01T00:00:00Z")]);
    assert!((cost - 0.0122).abs() < 1e-12);
}

#[test]
fn does_not_price_overlapping_write_counts_as_valid_lifetimes() {
    let usage = json!({
        "input_tokens": 100, "output_tokens": 0,
        "input_tokens_details": {
            "cached_tokens": 80, "cache_write_tokens": 40,
            "cache_write_tokens_details": { "ttl_30m_tokens": 0, "ttl_12h_tokens": 40 }
        }
    });
    let cost = standard_cost(&[response(usage, "2026-10-01T00:00:00Z")]);
    assert!((cost - 0.000033).abs() < 1e-12);
}

#[test]
fn keeps_flat_write_pricing_for_missing_or_invalid_lifetime_details() {
    for details in [
        Value::Null,
        json!({}),
        json!({ "ttl_30m_tokens": 3_000 }),
        json!({ "ttl_30m_tokens": 3_000, "ttl_12h_tokens": 3_000 }),
        json!({ "ttl_30m_tokens": -1, "ttl_12h_tokens": 7_001 }),
        json!({ "ttl_30m_tokens": "3000", "ttl_12h_tokens": 4_000 }),
        json!({ "ttl_30m_tokens": 3_000.5, "ttl_12h_tokens": 3_999.5 }),
        json!({ "ttl_30m_tokens": u64::MAX, "ttl_12h_tokens": 1 }),
        json!([]),
    ] {
        let cost = standard_cost(&[response(
            usage_with_details(details.clone()),
            "2026-10-01T00:00:00Z",
        )]);
        assert!((cost - 0.01035).abs() < 1e-12, "details: {details}");
    }
}

#[test]
fn retains_legacy_json_and_cost_without_lifetime_metadata() {
    let legacy = json!({
        "input_tokens": 12_000, "cached_input_tokens": 4_000,
        "cache_write_input_tokens": 7_000, "output_tokens": 100, "total_tokens": 12_100,
    });
    let speed = CodexSpeedPolicy::Forced(CodexServiceTier::Standard);
    let old = report_lines(
        &[response(legacy, "2026-10-01T00:00:00Z")],
        &test_pricing(),
        speed,
    );
    let nested = report_lines(
        &[response(
            usage_with_details(Value::Null),
            "2026-10-01T00:00:00Z",
        )],
        &test_pricing(),
        speed,
    );
    assert_eq!(old, nested);
}

#[test]
fn mixes_known_lifetimes_with_unclassified_write_overrides() {
    let mut pricing = test_pricing();
    pricing.load_json(
        r#"{
        "gpt-5.6-terra": {
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000002,
            "cache_read_input_token_cost": 0.0000001,
            "cache_creation_input_token_cost": 0.000003
        }
    }"#,
    );
    let report = report_lines(
        &[
            response(
                usage_with_details(json!({"ttl_30m_tokens": 3_000, "ttl_12h_tokens": 4_000})),
                "2026-10-01T00:00:00Z",
            ),
            response(usage_with_details(Value::Null), "2026-10-01T00:01:00Z"),
        ],
        &pricing,
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
    );
    // The known request costs $0.01535; unclassified writes use the $3/M override.
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.03795).abs() < 1e-12);
    assert_eq!(report["totals"]["cacheCreationTokens"], 14_000);
    assert_eq!(report["totals"]["totalTokens"], 24_200);
}

fn cumulative_usage(timestamp: &str, input: u64, details: Value) -> Value {
    json!({
        "type": "event_msg", "timestamp": timestamp,
        "payload": { "type": "token_count", "info": {
            "model": "gpt-5.6-terra", "total_token_usage": {
                "input_tokens": input, "cached_input_tokens": input * 2 / 5,
                "cache_write_input_tokens": input / 2, "output_tokens": 0,
                "input_tokens_details": { "cache_write_tokens_details": details },
                "total_tokens": input,
            },
        }},
    })
}

#[test]
fn prices_lifetime_deltas_from_cumulative_usage() {
    let cost = standard_cost(&[
        cumulative_usage(
            "2026-10-01T00:00:00Z",
            1_000,
            json!({"ttl_30m_tokens":100,"ttl_12h_tokens":400}),
        ),
        cumulative_usage(
            "2026-10-01T00:01:00Z",
            2_000,
            json!({"ttl_30m_tokens":200,"ttl_12h_tokens":800}),
        ),
    ]);
    assert!((cost - 0.00253).abs() < 1e-12);
}

#[test]
fn does_not_assign_new_cumulative_lifetimes_to_previously_unclassified_writes() {
    let cost = standard_cost(&[
        cumulative_usage("2026-10-01T00:00:00Z", 1_000, Value::Null),
        cumulative_usage(
            "2026-10-01T00:01:00Z",
            2_000,
            json!({"ttl_30m_tokens":200,"ttl_12h_tokens":800}),
        ),
    ]);
    assert!((cost - 0.00153).abs() < 1e-12);
}

#[test]
fn does_not_recount_a_snapshot_when_only_its_lifetime_metadata_changes() {
    let mut first = cumulative_usage("2026-10-01T00:00:00Z", 1_000, Value::Null);
    first["payload"]["info"]["last_token_usage"] =
        first["payload"]["info"]["total_token_usage"].clone();
    let mut repeated = cumulative_usage(
        "2026-10-01T00:01:00Z",
        1_000,
        json!({"ttl_30m_tokens":100,"ttl_12h_tokens":400}),
    );
    repeated["payload"]["info"]["last_token_usage"] =
        repeated["payload"]["info"]["total_token_usage"].clone();
    let report = report_lines(
        &[first, repeated],
        &test_pricing(),
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
    );
    assert_eq!(report["totals"]["totalTokens"], 1_000);
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.000765).abs() < 1e-12);
}

#[test]
fn keeps_lifetime_costs_in_recorded_service_tier_buckets() {
    let usage = usage_with_details(json!({ "ttl_30m_tokens": 3_000, "ttl_12h_tokens": 4_000 }));
    let mut lines = Vec::new();
    for (minute, tier) in [(0, "default"), (1, "priority"), (2, "flex")] {
        let timestamp = format!("2026-10-01T00:0{minute}:00Z");
        lines.push(json!({
            "type": "event_msg", "timestamp": timestamp,
            "payload": { "type": "thread_settings_applied", "thread_settings": { "service_tier": tier } }
        }));
        lines.push(json!({
            "type": "event_msg", "timestamp": timestamp,
            "payload": { "type": "token_count", "info": { "model": "gpt-5.6-terra", "last_token_usage": usage } }
        }));
    }
    let report = report_lines(
        &lines,
        &test_pricing(),
        CodexSpeedPolicy::Auto(CodexServiceTier::Standard),
    );
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.053725).abs() < 1e-12);
}

#[test]
fn retains_lifetime_costs_in_matched_compaction_usage() {
    let report = report_lines(
        &[
            json!({
                "type": "token_usage_record", "timestamp": "2026-10-01T00:00:00Z",
                "payload": {
                    "response_id": "ttl-compaction", "model": "gpt-5.6-terra",
                    "usage": usage_with_details(json!({ "ttl_30m_tokens": 3_000, "ttl_12h_tokens": 4_000 }))
                }
            }),
            json!({
                "type": "compacted", "timestamp": "2026-10-01T00:01:00Z",
                "payload": { "compaction_response_id": "ttl-compaction" }
            }),
        ],
        &test_pricing(),
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
    );
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.01535).abs() < 1e-12);
    assert_eq!(report["totals"]["totalTokens"], 12_100);
}

#[test]
fn preserves_compaction_lifetimes_without_double_counting_a_flat_token_snapshot() {
    let flat = json!({
        "input_tokens": 12_000, "cached_input_tokens": 4_000,
        "cache_write_input_tokens": 7_000, "output_tokens": 100, "total_tokens": 12_100
    });
    let report = report_lines(
        &[
            json!({
                "type": "token_usage_record", "timestamp": "2026-10-01T00:00:00Z",
                "payload": {
                    "response_id": "ttl-compaction", "model": "gpt-5.6-terra",
                    "usage": usage_with_details(json!({ "ttl_30m_tokens": 3_000, "ttl_12h_tokens": 4_000 }))
                }
            }),
            json!({
                "type": "event_msg", "timestamp": "2026-10-01T00:01:00Z",
                "payload": { "type": "token_count", "info": {
                    "model": "gpt-5.6-terra", "last_token_usage": flat, "total_token_usage": flat
                }}
            }),
            json!({
                "type": "compacted", "timestamp": "2026-10-01T00:02:00Z",
                "payload": { "compaction_response_id": "ttl-compaction" }
            }),
        ],
        &test_pricing(),
        CodexSpeedPolicy::Forced(CodexServiceTier::Standard),
    );
    assert_eq!(report["totals"]["totalTokens"], 12_100);
    assert!((report["totals"]["costUSD"].as_f64().unwrap() - 0.01535).abs() < 1e-12);
}

#[test]
fn prices_lifetimes_at_the_request_context_and_service_tier_rates() {
    let mut pricing = test_pricing();
    pricing.load_json(
        r#"{
        "gpt-5.6-terra": {
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000002,
            "cache_read_input_token_cost": 0.0000001,
            "cache_creation_input_token_cost": 0.00000125,
            "input_cost_per_token_above_200k_tokens": 0.000002,
            "output_cost_per_token_above_200k_tokens": 0.000003,
            "cache_read_input_token_cost_above_200k_tokens": 0.0000002,
            "cache_creation_input_token_cost_above_200k_tokens": 0.0000025,
            "provider_specific_entry": { "fast": 2, "flex": 0.5 }
        }
    }"#,
    );
    let usage = json!({
        "input_tokens": 300_000, "output_tokens": 100,
        "input_tokens_details": {
            "cached_tokens": 40_000, "cache_write_tokens": 250_000,
            "cache_write_tokens_details": { "ttl_30m_tokens": 150_000, "ttl_12h_tokens": 100_000 }
        },
    });
    for (tier, expected) in [
        (CodexServiceTier::Standard, 0.9033),
        (CodexServiceTier::Fast, 1.8066),
        (CodexServiceTier::Flex, 0.45165),
    ] {
        let report = report_lines(
            &[response(usage.clone(), "2026-10-01T00:00:00Z")],
            &pricing,
            CodexSpeedPolicy::Forced(tier),
        );
        assert!(
            (report["totals"]["costUSD"].as_f64().unwrap() - expected).abs() < 1e-12,
            "tier: {tier:?}"
        );
    }
}
