use crate::tools::error::{AppError, AppResult};
use serde_json::Value;
use uuid::Uuid;

pub fn gen_random_id() -> String {
    Uuid::new_v4().to_string()
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
