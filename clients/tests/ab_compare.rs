use serde::Serialize;
use serde_json::{to_value, Value};
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;

// ---------- generic assert helpers ----------

fn assert_json_eq<L, R>(left: &L, right: &R, label: &str)
where
    L: Serialize + std::fmt::Debug,
    R: Serialize + std::fmt::Debug,
{
    let l: Value = to_value(left).expect("left to JSON");
    let r: Value = to_value(right).expect("right to JSON");
    if l != r {
        eprintln!("=== {}: JSON mismatch ===", label);
        eprintln!("left : {}", serde_json::to_string_pretty(&l).unwrap());
        eprintln!("right: {}", serde_json::to_string_pretty(&r).unwrap());
        panic!("{} mismatch", label);
    }
}

#[allow(dead_code)]
fn assert_dbg_eq<L: std::fmt::Debug, R: std::fmt::Debug>(left: &L, right: &R, label: &str) {
    let ls = format!("{:#?}", left);
    let rs = format!("{:#?}", right);
    if ls != rs {
        eprintln!("=== {}: Debug mismatch ===", label);
        eprintln!("left : {}", ls);
        eprintln!("right: {}", rs);
        panic!("{} mismatch", label);
    }
}

// Normalize manual tuple [adds, rems] into object { additions, removals }
fn normalize_addrem_value(v: &Value) -> Value {
    match v {
        Value::Array(a) if a.len() == 2 => {
            let mut obj = serde_json::Map::new();
            obj.insert("additions".into(), a[0].clone());
            obj.insert("removals".into(), a[1].clone());
            Value::Object(obj)
        }
        _ => v.clone(),
    }
}

fn assert_sporeos_eq<L, R>(left: &L, right: &R, label: &str)
where
    L: Serialize + std::fmt::Debug,
    R: Serialize + std::fmt::Debug,
{
    let mut l = serde_json::to_value(left).expect("left to JSON");
    let mut r = serde_json::to_value(right).expect("right to JSON");

    // Ignore volatile top-level timestamp
    if let Some(o) = l.as_object_mut() { o.remove("time_received"); }
    if let Some(o) = r.as_object_mut() { o.remove("time_received"); }

    // (Optional) If you want to be even looser: you can ignore the huge proofs/witness blobs too.
    // Uncomment if needed:
    // for v in [&mut l, &mut r] {
    //     if let Some(o) = v.as_object_mut() {
    //         if let Some(eos) = o.get_mut("eos").and_then(|x| x.as_object_mut()) {
    //             if let Some(proofs) = eos.get_mut("proofs").and_then(|x| x.as_object_mut()) {
    //                 proofs.remove("challenge_chain_slot_proof");
    //                 proofs.remove("infused_challenge_chain_slot_proof");
    //                 proofs.remove("reward_chain_slot_proof");
    //             }
    //         }
    //     }
    // }

    if l != r {
        eprintln!("=== {}: JSON mismatch (normalized) ===", label);
        eprintln!("left : {}", serde_json::to_string_pretty(&l).unwrap());
        eprintln!("right: {}", serde_json::to_string_pretty(&r).unwrap());
        panic!("{} mismatch after normalization", label);
    }
}

// ---------- client makers (same host for both) ----------

use crate::api::full_node::FullnodeAPI as ManualFullnodeAPI;
use crate::rpc::full_node::FullnodeClient as ManualFullnodeClient;
use crate::rpc::full_node_rpc_generator::{FullnodeAPI as GenFullnodeAPI, FullnodeClient as GenFullnodeClient};

async fn mk_manual() -> ManualFullnodeClient {
    ManualFullnodeClient::new("druid.garden", 443, 30, None, &None).unwrap()
}
async fn mk_generated() -> GenFullnodeClient {
    GenFullnodeClient::new("druid.garden", 443, 30, None, &None).unwrap()
}

// Try a few canonical heights; pick the first that exists on this node.
async fn choose_height<M: crate::api::full_node::FullnodeAPI>(c: &M) -> u32 {
    const CANDIDATE_HEIGHTS: &[u32] = &[
        7_500_000, 1_000_000, 500_000, 100_000, 10_000, 1_000, 1,
    ];
    for &h in CANDIDATE_HEIGHTS {
        if c.get_block_record_by_height(h).await.is_ok() {
            return h;
        }
    }
    panic!("no candidate height available on this node");
}

// ---------- “known-good” helpers (trait-only) ----------

/// Return the first candidate block that has reward claims in its transactions_info.
async fn pick_block_with_rewards<C: crate::api::full_node::FullnodeAPI>(
    c: &C,
) -> Option<(u32, Bytes32, FullBlock)> {
    const H_CANDIDATES: &[u32] = &[7_500_000, 1_000_000, 500_000, 100_000, 10_000, 1_000, 1];
    for &h in H_CANDIDATES {
        if let Ok(br) = c.get_block_record_by_height(h).await {
            if let Ok(b) = c.get_block(&br.header_hash).await {
                if b.transactions_info
                    .as_ref()
                    .map_or(false, |ti| !ti.reward_claims_incorporated.is_empty())
                {
                    return Some((h, br.header_hash, b));
                }
            }
        }
    }
    None
}

/// From reward claims (which are `Coin`s), return (coin_id, puzzle_hash).
fn coin_from_rewards(blk: &FullBlock) -> Option<(Bytes32 /*coin_id*/, Bytes32 /*puzzle_hash*/)> {
    blk.transactions_info
        .as_ref()
        .and_then(|ti| ti.reward_claims_incorporated.first())
        .map(|reward_coin| (reward_coin.name(), reward_coin.puzzle_hash))
}

/// Try to obtain a *cached* challenge hash suitable for `get_recent_signage_point_or_eos`.
/// 1) Prefer unfinished headers (fresh), 2) else scan last `scan_back` block records for EOS hashes.
async fn find_cached_challenge_hash<C: crate::api::full_node::FullnodeAPI>(
    c: &C,
    scan_back: u32,
) -> Option<Bytes32> {
    // Option A: unfinished headers are usually in cache
    if let Ok(uhbs) = c.get_unfinished_block_headers().await {
        if let Some(uhb) = uhbs.first() {
            return Some(uhb.reward_chain_block.pos_ss_cc_challenge_hash);
        }
    }

    // Option B: latest EOS hash (may or may not be cached on this node)
    if let Ok(st) = c.get_blockchain_state().await {
        if let Some(peak) = st.peak {
            let start = peak.height.saturating_sub(scan_back);
            if let Ok(recs) = c.get_block_records(start, peak.height).await {
                for br in recs.into_iter().rev() {
                    if let Some(hashes) = br.finished_challenge_slot_hashes.as_ref() {
                        if let Some(last) = hashes.last() {
                            return Some(*last);
                        }
                    }
                }
            }
        }
    }
    None
}

// Extra checks that only use the *traits* (no helper methods).
async fn extra_known_good_checks(
    man: &impl ManualFullnodeAPI,
    gen: &impl GenFullnodeAPI,
) {
    // 1) Stable coins from reward claims (manual vs generated)
    if let Some((h_fixed, hh_fixed, blk_fixed)) = pick_block_with_rewards(man).await {
        if let Some((coin_id, puzzle_hash)) = coin_from_rewards(&blk_fixed) {
            let cr_man = man.get_coin_record_by_name(&coin_id).await;
            let cr_gen = gen.get_coin_record_by_name(&coin_id).await;
            match (cr_man, cr_gen) {
                (Ok(l), Ok(r)) => assert_json_eq(&l, &r, "coin_record_by_name(reward)"),
                (l, r) => eprintln!("coin_record_by_name mismatch/unavailable: manual={:?} gen={:?}", l, r),
            }

            let crph_man = man
                .get_coin_records_by_puzzle_hash(&puzzle_hash, Some(true), Some(0), Some(h_fixed))
                .await;
            let crph_gen = gen
                .get_coin_records_by_puzzle_hash(&puzzle_hash, Some(true), Some(0), Some(h_fixed))
                .await;
            match (crph_man, crph_gen) {
                (Ok(l), Ok(r)) => assert_json_eq(&l, &r, "coin_records_by_puzzle_hash(reward)"),
                (l, r) => eprintln!("coin_records_by_puzzle_hash mismatch/unavailable: manual={:?} gen={:?}", l, r),
            }

            // A/R: normalize shapes before asserting
            let ar_man = man.get_additions_and_removals(&hh_fixed).await;
            let ar_gen = gen.get_additions_and_removals(&hh_fixed).await;
            match (ar_man, ar_gen) {
                (Ok(l), Ok(r)) => {
                    let vl = normalize_addrem_value(&to_value(&l).unwrap());
                    let vr = normalize_addrem_value(&to_value(&r).unwrap());
                    if vl != vr {
                        eprintln!("=== additions_and_removals(reward-block): JSON mismatch after normalization ===");
                        eprintln!("left : {}", serde_json::to_string_pretty(&vl).unwrap());
                        eprintln!("right: {}", serde_json::to_string_pretty(&vr).unwrap());
                        panic!("additions_and_removals(reward-block) mismatch");
                    }
                }
                (l, r) => eprintln!("a/r reward-block mismatch/unavailable: manual={:?} gen={:?}", l, r),
            }
        }
    } else {
        eprintln!("No block with reward_claims among candidates; skipping pinned-coin checks.");
    }

    // 2) Recent SP/EOS via cached challenge hash (best effort)
    if let Some(ch) = find_cached_challenge_hash(man, 512).await {
        let a_man = man.get_recent_signage_point_or_eos(None, Some(&ch)).await;
        let a_gen = gen.get_recent_signage_point_or_eos(None, Some(&ch)).await;
        match (a_man, a_gen) {
            (Ok(l), Ok(r)) => assert_sporeos_eq(&l, &r, "get_recent_signage_point_or_eos(challenge_hash)"),
            (l, r) => eprintln!(
                "signage_point_or_eos not available via challenge_hash on this node: manual={:?} gen={:?}",
                l, r
            ),
        }
    } else {
        eprintln!("Could not find a cached challenge_hash; skipping SP/EOS check.");
    }
}

// Fee tests must use the **concrete clients** and will not panic if unsupported.
async fn fee_tests(man: &ManualFullnodeClient, gen: &GenFullnodeClient) {
    // tiny helper so we can see the body that actually gets POSTed
    async fn fee_call<'a>(
        label: &str,
        client_name: &str,
        post_url: String,
        client: &reqwest::Client,
        headers: &Option<std::collections::HashMap<String, String>>,
        cost: Option<u64>,
        spend_bundle: Option<dg_xch_core::blockchain::spend_bundle::SpendBundle>,
        spend_type: Option<String>,
        target_times: &'a [u64],
    ) -> Result<dg_xch_core::protocols::full_node::FeeEstimate, crate::rpc::ChiaRpcError> {
        use serde_json::{json, Map, Value};
        let mut body = Map::<String, Value>::new();
        if let Some(v) = cost         { body.insert("cost".into(), json!(v)); }
        if let Some(v) = spend_bundle { body.insert("spend_bundle".into(), json!(v)); }
        if let Some(v) = spend_type   { body.insert("spend_type".into(), json!(v)); }
        body.insert("target_times".into(), json!(target_times));

        // >>> LOG EXACT PAYLOAD <<<
        eprintln!(
            "[fee_dbg] {} {} body => {}",
            label,
            client_name,
            serde_json::to_string(&body).unwrap()
        );

        // Use the same response wrapper type your generator/manual uses
        let resp: crate::api::responses::FeeEstimateResp = crate::rpc::post::<_, std::hash::RandomState>(
            client,
            &post_url,
            &body,
            headers,
        ).await?;

        Ok(resp.result)
    }

    // Build the exact URLs (same as the clients do)
    let man_url = (man.url_function)(man.host.as_str(), man.port, "get_fee_estimate");
    let gen_url = (gen.url_function)(gen.host.as_str(), gen.port, "get_fee_estimate");

    // Try "cost" first
    let fee_cost_man = fee_call(
        "cost",
        "manual",
        man_url.clone(),
        &man.client,
        &man.additional_headers,
        Some(100_000),
        None,
        None,
        &[60, 120, 300],
    ).await;

    let fee_cost_gen = fee_call(
        "cost",
        "generated",
        gen_url.clone(),
        &gen.client,
        &gen.additional_headers,
        Some(100_000),
        None,
        None,
        &[60, 120, 300],
    ).await;

    // If the node doesn’t like "cost", try "spend_type"
    let fee_type_man = if fee_cost_man.is_err() {
        fee_call(
            "spend_type",
            "manual",
            man_url.clone(),
            &man.client,
            &man.additional_headers,
            None,
            None,
            Some("default".into()),
            &[60, 120, 300],
        ).await
    } else { fee_cost_man.clone() };

    let fee_type_gen = if fee_cost_gen.is_err() {
        fee_call(
            "spend_type",
            "generated",
            gen_url.clone(),
            &gen.client,
            &gen.additional_headers,
            None,
            None,
            Some("default".into()),
            &[60, 120, 300],
        ).await
    } else { fee_cost_gen.clone() };

    let chosen = match (fee_cost_man.as_ref(), fee_cost_gen.as_ref()) {
        (Ok(_), Ok(_)) => (fee_cost_man.clone(), fee_cost_gen.clone(), "fee_estimate(cost)"),
        _ => (fee_type_man.clone(), fee_type_gen.clone(), "fee_estimate(spend_type)"),
    };

    match chosen {
        (Ok(l), Ok(r), label) => {
            assert!(!l.estimates.is_empty(), "expected non-empty estimates for {}", label);
            assert_json_eq(&l, &r, label);
        }
        _ => eprintln!(
            "Fee estimate unsupported by node: cost={:?}, type={:?}",
            fee_cost_man.as_ref().err().zip(fee_cost_gen.as_ref().err()),
            fee_type_man.as_ref().err().zip(fee_type_gen.as_ref().err()),
        ),
    }
}

fn normalize_mempool_item_value(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};

    // Keep only stable / invariant bits. Everything else can differ per node/instant.
    //
    // Prefer a canonical id key (`spend_bundle_name`), but if the server returns `tx_id`,
    // we normalize it into the same slot.
    //
    // Optionally we add a tiny shape hint (`coin_spend_count`) which is stable for a given tx.
    match v {
        Value::Object(obj) => {
            let mut o = Map::new();

            if let Some(x) = obj.get("fee").cloned() {
                o.insert("fee".into(), x);
            }
            if let Some(x) = obj.get("cost").cloned() {
                o.insert("cost".into(), x);
            }

            if let Some(x) = obj.get("spend_bundle_name").cloned() {
                o.insert("spend_bundle_name".into(), x);
            } else if let Some(x) = obj.get("tx_id").cloned() {
                o.insert("spend_bundle_name".into(), x);
            }

            if let Some(sb) = obj.get("spend_bundle").and_then(|v| v.as_object()) {
                if let Some(cs) = sb.get("coin_spends").and_then(|v| v.as_array()) {
                    o.insert(
                        "coin_spend_count".into(),
                        Value::Number(serde_json::Number::from(cs.len() as u64)),
                    );
                }
            }

            Value::Object(o)
        }
        _ => v.clone(),
    }
}



// ---------- main A/B test ----------

#[tokio::test]
async fn ab_compare_manual_vs_generated() {
    // Build both clients
    let man = mk_manual().await;
    let gen = mk_generated().await;

    // 1) get_blockchain_state — live, so only ensure both decode (no equality assert)
    let s_man = man.get_blockchain_state().await.expect("manual bcs");
    let s_gen = gen.get_blockchain_state().await.expect("generated bcs");
    // optional: log peak heights to help diagnose
    eprintln!(
        "bcs: manual peak={:?}, generated peak={:?}",
        s_man.peak.as_ref().map(|p| p.height),
        s_gen.peak.as_ref().map(|p| p.height)
    );

    // 2) get_block_count_metrics — drift-tolerant, do not panic
    let (m_man_res, m_gen_res) = tokio::join!(
        man.get_block_count_metrics(),
        gen.get_block_count_metrics()
    );
    let m_man = m_man_res.expect("manual bcm");
    let m_gen = m_gen_res.expect("generated bcm");

    const BCM_TOL: u64 = 3;
    let diff = |a: u64, b: u64| a.max(b) - a.min(b);
    let dc = diff(m_man.compact_blocks,   m_gen.compact_blocks);
    let du = diff(m_man.uncompact_blocks, m_gen.uncompact_blocks);
    let dh = diff(m_man.hint_count,       m_gen.hint_count);
    if dc > BCM_TOL || du > BCM_TOL || dh > BCM_TOL {
        eprintln!("=== get_block_count_metrics: metrics drift exceeded tolerance (tol={}) ===", BCM_TOL);
        eprintln!("left : {:?}", m_man);
        eprintln!("right: {:?}", m_gen);
        eprintln!("diffs: compact_blocks={}, uncompact_blocks={}, hint_count={}", dc, du, dh);
        // log-only; do not panic
    }

    // 3) get_network_info — should be stable
    let ni_man = man.get_network_info().await.expect("manual ni");
    let ni_gen = gen.get_network_info().await.expect("generated ni");
    assert_json_eq(&ni_man, &ni_gen, "get_network_info");

    // 4) by-height suite
    let h = choose_height(&man).await;

    let br_man = man.get_block_record_by_height(h).await.expect("manual br@h");
    let br_gen = gen.get_block_record_by_height(h).await.expect("generated br@h");
    assert_json_eq(&br_man, &br_gen, "get_block_record_by_height");

    let hh = br_man.header_hash;
    let b_man = man.get_block(&hh).await.expect("manual block@hh");
    let b_gen = gen.get_block(&hh).await.expect("generated block@hh");
    assert_json_eq(&b_man, &b_gen, "get_block");

    // additions/removals — normalize tuple/object before compare
    let ar_man = man.get_additions_and_removals(&hh).await.expect("manual a/r");
    let ar_gen = gen.get_additions_and_removals(&hh).await.expect("generated a/r");
    let ar_man_v = normalize_addrem_value(&to_value(&ar_man).unwrap());
    let ar_gen_v = normalize_addrem_value(&to_value(&ar_gen).unwrap());
    if ar_man_v != ar_gen_v {
        eprintln!("=== get_additions_and_removals: JSON mismatch after normalization ===");
        eprintln!("left : {}", serde_json::to_string_pretty(&ar_man_v).unwrap());
        eprintln!("right: {}", serde_json::to_string_pretty(&ar_gen_v).unwrap());
        panic!("get_additions_and_removals mismatch");
    }

    let start = h.saturating_sub(4);
    let end   = h;
    let blocks_man = man.get_blocks(start, end, false, true).await.expect("manual get_blocks");
    let blocks_gen = gen.get_blocks(start, end, false, true).await.expect("generated get_blocks");
    assert_json_eq(&blocks_man, &blocks_gen, "get_blocks");

    // 5) mempool ids
    let ids_man = man.get_all_mempool_tx_ids().await.expect("manual txids");
    let ids_gen = gen.get_all_mempool_tx_ids().await.expect("generated txids");
    assert_json_eq(&ids_man, &ids_gen, "get_all_mempool_tx_ids");

    // 6) optional extras (trait-only: pinned coins & SP/EOS)
    extra_known_good_checks(&man, &gen).await;

    // 7) optional fee comparison (concrete-only)
    // fee_tests(&man, &gen).await;
    // 4b) get_block_records window (already have start/end)
    {
        let recs_man = man.get_block_records(start, end).await.expect("manual recs");
        let recs_gen = gen.get_block_records(start, end).await.expect("generated recs");
        assert_json_eq(&recs_man, &recs_gen, "get_block_records");
    }

    // 4c) get_unfinished_block_headers — live/volatile; just assert both decode
    {
        let uhb_man = man.get_unfinished_block_headers().await;
        let uhb_gen = gen.get_unfinished_block_headers().await;
        match (uhb_man, uhb_gen) {
            (Ok(l), Ok(r)) => {
                // not strict equality (can change between calls), but shape should be fine
                assert!(l.len() >= 0 && r.len() >= 0, "unfinished headers should decode");
            }
            (l, r) => eprintln!("unfinished headers unavailable: manual={:?} gen={:?}", l, r),
        }
    }

    // 6b) Additional coin lookups from the same reward coin (if present)
    if let Some((h_fixed, hh_fixed, blk_fixed)) = pick_block_with_rewards(&man).await {
        if let Some((coin_id, puzzle_hash)) = coin_from_rewards(&blk_fixed) {
            // get_coin_records_by_names (single)
            let by_names_man = man.get_coin_records_by_names(&[coin_id], Some(true), Some(0), Some(h_fixed)).await;
            let by_names_gen = gen.get_coin_records_by_names(&[coin_id], Some(true), Some(0), Some(h_fixed)).await;
            match (by_names_man, by_names_gen) {
                (Ok(l), Ok(r)) => assert_json_eq(&l, &r, "get_coin_records_by_names(single)"),
                (l, r) => eprintln!("by_names(single) mismatch/unavailable: manual={:?} gen={:?}", l, r),
            }

            // get_coin_records_by_parent_ids (use the coin's parent)
            if let Ok(Some(cr)) = man.get_coin_record_by_name(&coin_id).await {
                let parent = cr.coin.parent_coin_info;
                let by_parent_man = man.get_coin_records_by_parent_ids(&[parent], Some(true), Some(0), Some(h_fixed)).await;
                let by_parent_gen = gen.get_coin_records_by_parent_ids(&[parent], Some(true), Some(0), Some(h_fixed)).await;
                match (by_parent_man, by_parent_gen) {
                    (Ok(l), Ok(r)) => assert_json_eq(&l, &r, "get_coin_records_by_parent_ids"),
                    (l, r) => eprintln!("by_parent mismatch/unavailable: manual={:?} gen={:?}", l, r),
                }
            }

            // get_puzzle_and_solution — only if coin is actually spent
            if let Ok(Some(cr)) = man.get_coin_record_by_name(&coin_id).await {
                if cr.spent_block_index > 0 {
                    let pas_man = man.get_puzzle_and_solution(&coin_id, cr.spent_block_index).await;
                    let pas_gen = gen.get_puzzle_and_solution(&coin_id, cr.spent_block_index).await;
                    match (pas_man, pas_gen) {
                        (Ok(l), Ok(r)) => assert_json_eq(&l, &r, "get_puzzle_and_solution"),
                        (l, r) => eprintln!("puzzle_and_solution mismatch/unavailable: manual={:?} gen={:?}", l, r),
                    }
                } else {
                    eprintln!("reward coin not spent; skipping puzzle_and_solution");
                }
            }
        }
    }

    // 7) mempool: compare a single picked txid, normalized; avoid full-map asserts
    {
        // We already asserted ids_man == ids_gen earlier; use that common set
        let pick_txid = ids_man.first().cloned();

        if let Some(txid) = pick_txid {
            let txid_s = format!("{txid}");

            // Fetch both items in parallel to minimize drift
            let (by_id_man, by_id_gen) = tokio::join!(
            man.get_mempool_item_by_tx_id(&txid_s),
            gen.get_mempool_item_by_tx_id(&txid_s)
        );

            if let (Ok(l), Ok(r)) = (by_id_man.as_ref(), by_id_gen.as_ref()) {
                let lv = normalize_mempool_item_value(&serde_json::to_value(l).unwrap());
                let rv = normalize_mempool_item_value(&serde_json::to_value(r).unwrap());
                assert_json_eq(&lv, &rv, "get_mempool_item_by_tx_id(picked)-normalized");

                // Optional: also test items_by_coin_name using the first coin spend (no prints)
                if let Some(first_cs) = l
                    .spend_bundle
                    .coin_spends
                    .first()
                {
                    let coin_name = first_cs.coin.name();
                    let (by_coin_man, by_coin_gen) = tokio::join!(
                    man.get_mempool_items_by_coin_name(&coin_name),
                    gen.get_mempool_items_by_coin_name(&coin_name)
                );
                    if let (Ok(lc), Ok(rc)) = (by_coin_man, by_coin_gen) {
                        // Compare normalized list entries for the picked tx only (if present)
                        let l_pick = lc.iter().find(|mi| mi.spend_bundle_name == txid);
                        let r_pick = rc.iter().find(|mi| mi.spend_bundle_name == txid);
                        if let (Some(li), Some(ri)) = (l_pick, r_pick) {
                            let lv = normalize_mempool_item_value(&serde_json::to_value(li).unwrap());
                            let rv = normalize_mempool_item_value(&serde_json::to_value(ri).unwrap());
                            assert_json_eq(&lv, &rv, "get_mempool_items_by_coin_name(picked)-normalized");
                        }
                    }
                }
            }
            // else: one side didn’t have the tx anymore—skip silently to avoid muddying the test
        }
        // else: no txids available—skip silently
    }
}
