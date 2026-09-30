use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{Array, DictionaryArray};
use arrow_schema::{DataType, TimeUnit};
use datagrep_api::{Bytes, TzSpec, Value};

pub use datagrep_core::format::{value_to_json, CellText};

pub fn arrow_cell_to_value(array: &dyn Array, row: usize) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    match array.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(downcast::<arrow_array::BooleanArray>(array).value(row)),
        DataType::Int64 => Value::I64(downcast::<arrow_array::Int64Array>(array).value(row)),
        DataType::UInt64 => Value::U64(downcast::<arrow_array::UInt64Array>(array).value(row)),
        DataType::Float64 => Value::F64(downcast::<arrow_array::Float64Array>(array).value(row)),
        DataType::Date32 => Value::Date(downcast::<arrow_array::Date32Array>(array).value(row)),
        DataType::Time64(TimeUnit::Nanosecond) => Value::Time {
            nanos: downcast::<arrow_array::Time64NanosecondArray>(array).value(row),
        },
        DataType::Timestamp(TimeUnit::Microsecond, tz) => Value::Timestamp {
            micros: downcast::<arrow_array::TimestampMicrosecondArray>(array).value(row),
            tz: tz_spec(tz.as_deref()),
        },
        DataType::FixedSizeBinary(16) => {
            let a = downcast::<arrow_array::FixedSizeBinaryArray>(array);
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(a.value(row));
            Value::Uuid(uuid)
        }
        DataType::Binary => {
            let a = downcast::<arrow_array::BinaryArray>(array);
            Value::Bytes(Bytes::copy_from_slice(a.value(row)))
        }
        DataType::Utf8 => Value::Str(Arc::from(
            downcast::<arrow_array::StringArray>(array).value(row),
        )),
        DataType::LargeUtf8 => Value::Str(Arc::from(
            downcast::<arrow_array::LargeStringArray>(array).value(row),
        )),
        DataType::Dictionary(key, value)
            if **key == DataType::Int32 && **value == DataType::Utf8 =>
        {
            let dict = downcast::<DictionaryArray<Int32Type>>(array);
            let keys = dict.keys();
            let values = dict
                .values()
                .as_any()
                .downcast_ref::<arrow_array::StringArray>();
            match values {
                Some(values) if !keys.is_null(row) => {
                    Value::Str(Arc::from(values.value(keys.value(row) as usize)))
                }
                _ => Value::Null,
            }
        }
        other => Value::Unsupported {
            type_name: Arc::from(format!("{other:?}")),
            raw: Bytes::new(),
            display: Arc::from("<unrenderable arrow type>"),
        },
    }
}

fn downcast<T: 'static>(array: &dyn Array) -> &T {
    array
        .as_any()
        .downcast_ref::<T>()
        .expect("arrow_cell_to_value's DataType match must agree with the concrete array type")
}

fn tz_spec(tz: Option<&str>) -> TzSpec {
    match tz {
        None => TzSpec::Naive,
        Some("UTC") => TzSpec::Utc,
        Some(name) => TzSpec::Named(Arc::from(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{Int64Builder, StringBuilder};
    use arrow_array::RecordBatch;
    use arrow_schema::{Field, Schema};

    #[test]
    fn arrow_cell_round_trips_int_and_string_columns() {
        let mut ints = Int64Builder::new();
        ints.append_value(7);
        ints.append_null();
        let mut strs = StringBuilder::new();
        strs.append_value("hi");
        strs.append_null();
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int64, true),
            Field::new("s", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(ints.finish()), Arc::new(strs.finish())],
        )
        .unwrap();

        assert_eq!(
            arrow_cell_to_value(batch.column(0).as_ref(), 0),
            Value::I64(7)
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(0).as_ref(), 1),
            Value::Null
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(1).as_ref(), 0),
            Value::Str(Arc::from("hi"))
        );
        assert_eq!(
            arrow_cell_to_value(batch.column(1).as_ref(), 1),
            Value::Null
        );
    }
}
