use crate::tools::error::{AppError, AppResult};
use serde_json::Value;
use uuid::Uuid;

pub fn gen_random_id() -> String {
    Uuid::new_v4().to_string()
}

/// Generate a compact, JS-safe `i64` id for auto-injecting PKs into id columns
/// that are still `bigint` (pre bigint->text migration).
///
/// Layout: `seconds_since_2024_epoch * 100_000 + 5_random_digits` (~13 digits).
/// - **Stays well under 2^53**, so it survives a round-trip through the JS frontend
///   (and old `Int64` consumers) without precision loss — which is why ids can keep
///   being sent as JSON numbers instead of strings.
/// - **Time-ordered** and far above the small sequence ids already in the column,
///   so it won't collide with existing data; the 5 random digits avoid clashes
///   within the same second. Gated by the `gen_int_for_id` config.
pub fn gen_random_int_id() -> i64 {
    const EPOCH_2024_SECS: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z
    let secs = (chrono::Utc::now().timestamp() - EPOCH_2024_SECS).max(0);
    let rand5 = (Uuid::new_v4().as_u128() as u64 % 100_000) as i64; // 0..99_999
    secs * 100_000 + rand5
}

pub fn generate_waybill_number() -> String {
    let timestamp = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let random_suffix = Uuid::new_v4()
        .to_string()
        .replace('-', "")
        .split_at(8)
        .0
        .to_string();
    format!("T{}{}", timestamp, random_suffix)
}

pub fn regenerate_field(field_name: &str) -> AppResult<Value> {
    match field_name {
        "waybill_no" => Ok(Value::String(generate_waybill_number())),
        _ => Err(AppError::BadRequest(format!(
            "Unknown regenerable field: '{}'",
            field_name
        ))),
    }
}

pub fn apply_regeneration(
    arr: &mut [Value],
    to_regen: &[String],
    table_columns: &[&str],
) -> AppResult<()> {
    for field in to_regen {
        if !table_columns.contains(&field.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Field '{}' not found in table columns",
                field
            )));
        }
    }

    for row in arr.iter_mut() {
        if let Some(obj) = row.as_object_mut() {
            for field in to_regen {
                let new_value = regenerate_field(field)?;
                obj.insert(field.clone(), new_value);
            }
        }
    }

    Ok(())
}
