//! Fast/priority price ratios verified against the OpenAI rate card, 2026-09-05.
use super::calculator::CostBreakdown;
use crate::error::AppError;
use rusqlite::{params, Connection};
use rust_decimal::Decimal;
use std::str::FromStr;

pub fn factors(model: &str, tier: Option<&str>, input_tokens: u64) -> Option<(Decimal, Decimal)> {
    if !matches!(tier, Some("fast" | "priority")) {
        return None;
    }
    // Exact model names and official dated snapshots only; never guess reseller aliases.
    let matches = |base: &str| {
        model == base
            || model.strip_prefix(base).is_some_and(|suffix| {
                suffix.len() == 11
                    && suffix.starts_with('-')
                    && chrono::NaiveDate::parse_from_str(&suffix[1..], "%Y-%m-%d").is_ok()
            })
    };
    let long = input_tokens > 272_000;
    if ["claude-opus-5", "claude-opus-4-8"]
        .iter()
        .any(|m| model == *m)
    {
        return (tier == Some("fast")).then_some((Decimal::from(2), Decimal::from(2)));
    }
    if [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ]
    .iter()
    .any(|m| matches(m))
    {
        return Some(if long {
            (Decimal::from(4), Decimal::from(3))
        } else {
            (Decimal::from(2), Decimal::from(2))
        });
    }
    if matches("gpt-5.5") && !long {
        return Some((Decimal::new(25, 1), Decimal::new(25, 1)));
    }
    if (matches("gpt-5.4") && !long)
        || ["gpt-5.4-mini", "gpt-5.3-codex", "gpt-5.2", "gpt-5.1"]
            .iter()
            .any(|m| matches(m))
    {
        return Some((Decimal::from(2), Decimal::from(2)));
    }
    None
}

pub fn apply(cost: &mut CostBreakdown, factors: (Decimal, Decimal), multiplier: Decimal) {
    cost.input_cost *= factors.0;
    cost.cache_read_cost *= factors.0;
    cost.cache_creation_cost *= factors.0;
    cost.output_cost *= factors.1;
    cost.total_cost =
        (cost.input_cost + cost.output_cost + cost.cache_read_cost + cost.cache_creation_cost)
            * multiplier;
}

/// Repair retained old rows once, using stored component costs to preserve historical rates.
/// Also used after session metadata enrichment; caller owns the transaction.
pub fn repair(conn: &Connection, request_id: Option<&str>) -> Result<u64, AppError> {
    let mut stmt = conn.prepare("SELECT request_id, COALESCE(NULLIF(pricing_model,''),model), service_tier,
        input_tokens, cache_read_tokens, cache_creation_tokens, input_token_semantics, app_type,
        input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd, cost_multiplier
        FROM proxy_request_logs WHERE service_tier_pricing_version=0 AND service_tier IN ('fast','priority') AND (?1 IS NULL OR request_id=?1)")?;
    let rows = stmt
        .query_map([request_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, String>(12)?,
                r.get::<_, String>(13)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut updated = 0;
    for (id, model, tier, input, read, write, semantics, app, ic, oc, rc, wc, total, multiplier) in
        rows
    {
        let mut context = input.max(0) as u64;
        if !crate::services::sql_helpers::is_cache_inclusive_app(&app) || semantics == 2 {
            context += read.max(0) as u64 + write.max(0) as u64;
        } else if semantics == 0 {
            context += write.max(0) as u64;
        }
        let Some(factors) = factors(&model, Some(&tier), context) else {
            continue;
        };
        let parsed = [ic, oc, rc, wc, total, multiplier]
            .iter()
            .map(|s| Decimal::from_str(s))
            .collect::<Result<Vec<_>, _>>();
        let Ok(v) = parsed else {
            continue;
        };
        if v.iter().any(|n| *n < Decimal::ZERO) || v[4] <= Decimal::ZERO {
            continue;
        }
        let mut cost = CostBreakdown {
            input_cost: v[0],
            output_cost: v[1],
            cache_read_cost: v[2],
            cache_creation_cost: v[3],
            total_cost: v[4],
        };
        // Explicit zero/custom totals are not silently replaced.
        if ((v[0] + v[1] + v[2] + v[3]) * v[5] - v[4]).abs() > Decimal::new(4, 6) {
            continue;
        }
        apply(&mut cost, factors, v[5]);
        updated += conn.execute("UPDATE proxy_request_logs SET input_cost_usd=?2,output_cost_usd=?3,cache_read_cost_usd=?4,cache_creation_cost_usd=?5,total_cost_usd=?6,service_tier_pricing_version=1 WHERE request_id=?1 AND service_tier_pricing_version=0",
            params![id,cost.input_cost.to_string(),cost.output_cost.to_string(),cost.cache_read_cost.to_string(),cost.cache_creation_cost.to_string(),cost.total_cost.to_string()])? as u64;
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_specific_factors_and_context_boundaries() {
        let two = Some((Decimal::from(2), Decimal::from(2)));
        assert_eq!(factors("gpt-6-astra", Some("priority"), 272_000), two);
        assert_eq!(
            factors("gpt-6-astra-2026-09-01", Some("fast"), 272_001),
            Some((Decimal::from(4), Decimal::from(3)))
        );
        assert_eq!(
            factors("gpt-5.5", Some("fast"), 100),
            Some((Decimal::new(25, 1), Decimal::new(25, 1)))
        );
        assert_eq!(factors("claude-opus-5", Some("fast"), 900_000), two);
        for (model, tier, context) in [
            ("gpt-6-astra", "default", 100),
            ("claude-opus-5", "priority", 100),
            ("claude-fable-5-1", "fast", 100),
            ("claude-opus-4-6", "fast", 100),
            ("gpt-5.5", "fast", 272_001),
            ("reseller-gpt-6-astra", "fast", 1),
            ("gpt-6-astra-2026-99-01", "fast", 1),
        ] {
            assert_eq!(factors(model, Some(tier), context), None, "{model}");
        }
    }

    #[test]
    fn astra_example_and_cache_multiplier_composition() {
        let mut cost = CostBreakdown {
            input_cost: Decimal::new(159191, 5),
            output_cost: Decimal::new(3, 4),
            cache_read_cost: Decimal::ZERO,
            cache_creation_cost: Decimal::ZERO,
            total_cost: Decimal::new(159221, 5),
        };
        apply(
            &mut cost,
            factors("gpt-6-astra", Some("fast"), 159191).unwrap(),
            Decimal::ONE,
        );
        assert_eq!(cost.total_cost, Decimal::new(318442, 5));
        cost.cache_read_cost = Decimal::new(5, 1);
        cost.cache_creation_cost = Decimal::ONE;
        apply(
            &mut cost,
            (Decimal::from(4), Decimal::from(3)),
            Decimal::new(15, 1),
        );
        assert_eq!(cost.cache_read_cost, Decimal::from(2));
        assert_eq!(cost.cache_creation_cost, Decimal::from(4));
        assert_eq!(
            cost.total_cost,
            (cost.input_cost + cost.output_cost + cost.cache_read_cost + cost.cache_creation_cost)
                * Decimal::new(15, 1)
        );
    }

    #[test]
    fn repair_preserves_historical_prices_tokens_and_is_idempotent() -> Result<(), AppError> {
        let db = crate::database::Database::memory()?;
        let conn = crate::database::lock_conn!(db.conn);
        for (id, tier) in [("fast", "priority"), ("standard", "default")] {
            conn.execute("INSERT INTO proxy_request_logs (request_id,provider_id,app_type,model,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_cost_usd,output_cost_usd,cache_read_cost_usd,cache_creation_cost_usd,total_cost_usd,cost_multiplier,service_tier,created_at,latency_ms,status_code) VALUES (?1,'p','codex','gpt-6-astra',159191,6,0,0,'1.59191','0.0003','0','0','2.388315','1.5',?2,100,0,200)", params![id,tier])?;
        }
        assert_eq!(repair(&conn, None)?, 1);
        assert_eq!(repair(&conn, None)?, 0);
        let row: (String,i64,i64,i64) = conn.query_row("SELECT total_cost_usd,input_tokens,output_tokens,created_at FROM proxy_request_logs WHERE request_id='fast'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        assert_eq!(Decimal::from_str(&row.0).unwrap(), Decimal::new(477663, 5));
        assert_eq!((row.1, row.2, row.3), (159191, 6, 100));
        let unchanged: String = conn.query_row(
            "SELECT total_cost_usd FROM proxy_request_logs WHERE request_id='standard'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(unchanged, "2.388315");
        Ok(())
    }
}
