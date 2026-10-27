//! Hand-computed cache cost example: three turns, one batch shrink, compactions.

use crate::view_tree::*;

fn input() -> CacheCostInput {
    let call = |output_bytes, tool_result_bytes| ModelCallBytes {
        output_bytes,
        tool_result_bytes,
    };
    CacheCostInput {
        prices: CachePrices {
            cache_read: 0.5,
            cache_write: 2.0,
            output: 4.0,
            compressor_ratio: 0.5,
            bytes_per_token: 1.0,
        },
        tools_system_bytes: 100,
        compaction: CompactionLimits {
            working_window_tokens: 1000,
            reserve_tokens: 200,
            keep_recent_tokens: 150,
            summary_bytes: 50,
        },
        turns: vec![
            TurnBytes {
                user_bytes: 100,
                calls: vec![call(50, 200), call(30, 0)],
            },
            TurnBytes {
                user_bytes: 100,
                calls: vec![call(50, 300), call(40, 0)],
            },
            TurnBytes {
                user_bytes: 60,
                calls: vec![call(20, 0)],
            },
        ],
        view_turns: vec![
            ViewTurnBytes {
                view_bytes: 0,
                shrink_unchanged_prefix_bytes: vec![],
            },
            ViewTurnBytes {
                view_bytes: 60,
                shrink_unchanged_prefix_bytes: vec![],
            },
            ViewTurnBytes {
                view_bytes: 100,
                shrink_unchanged_prefix_bytes: vec![30],
            },
        ],
        compressor: CompressorUsage {
            input_tokens: 1000,
            cached_tokens: 400,
            output_tokens: 100,
        },
    }
}

#[test]
fn current_layout_warm_matches_hand_computation() {
    // T1: 2*200+4*50 + (0.5*200+2*250+4*30) = 1320
    // T2: call 1 (0.5*450+2*130+4*50) = 685; call 2 compacts (input 930, 580 cached):
    //     summary 0.5*580+2*350+4*50 = 1190; request 450 with 100 cached: 50+700+160 = 910
    // T3: input 550, 450 cached: 225+200+80 = 505
    let cost = current_layout_cost(&input(), CacheState::Warm);
    assert_eq!(cost.total, 1320.0 + 685.0 + 1190.0 + 910.0 + 505.0);
    assert_eq!(cost.total, 4610.0);
    assert_eq!(cost.compactions, 1);
    assert_eq!(cost.per_turn, 4610.0 / 3.0);
    assert_eq!(
        cost.cached_input_tokens,
        200.0 + 450.0 + 580.0 + 100.0 + 450.0
    );
}

#[test]
fn current_layout_expired_applies_idle_compaction() {
    // T1 1320. T2: idle compaction (480 > 400): 2*480+4*50 = 1160; request 480, 100 cached:
    // 50+760+200 = 1010; call 2 compacts (830, 480 cached): 240+700+200 = 1140; request 450,
    // 100 cached: 50+700+160 = 910. T3: idle compaction (490): 980+200 = 1180; request 550,
    // 100 cached: 50+900+80 = 1030.
    let cost = current_layout_cost(&input(), CacheState::Expired);
    assert_eq!(
        cost.total,
        1320.0 + 1160.0 + 1010.0 + 1140.0 + 910.0 + 1180.0 + 1030.0
    );
    assert_eq!(cost.total, 7750.0);
    assert_eq!(cost.compactions, 3);
}

#[test]
fn view_layout_reads_only_the_view_prefix_no_shrink_rewrote() {
    // T1 1320. T2: call 1 input 260, 100 cached: 50+320+200 = 570; call 2 input 610, 260
    // cached: 130+700+160 = 990. T3: input 260, cached 100+min(60,30) = 130: 65+260+80 = 405.
    // Compressor: 0.5*(0.5*400 + 2*600 + 4*100) = 900.
    let cost = view_layout_cost(&input(), CacheState::Warm);
    assert_eq!(cost.main, 1320.0 + 570.0 + 990.0 + 405.0);
    assert_eq!(cost.compressor, 900.0);
    assert_eq!(cost.total, 4185.0);
    assert_eq!(cost.per_turn, 1395.0);
}

#[test]
fn view_layout_expired_writes_every_turn_start() {
    // T1 1320. T2: 2*260+200 = 720, call 2 (260 cached) 990. T3: 2*260+80 = 600.
    let cost = view_layout_cost(&input(), CacheState::Expired);
    assert_eq!(cost.main, 1320.0 + 720.0 + 990.0 + 600.0);
    assert_eq!(cost.total, 3630.0 + 900.0);
}

#[test]
fn report_holds_the_four_cases() {
    let report = cache_cost_report(&input());
    assert_eq!(report.current_warm.total, 4610.0);
    assert_eq!(report.view_expired.total, 4530.0);
    assert!(report.view_warm.per_turn < report.current_warm.per_turn);
}
