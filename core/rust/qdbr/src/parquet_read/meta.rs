use std::fs::File;

use crate::parquet_read::{ColumnChunkBuffers, ColumnMeta, ParquetDecoder};
use parquet2::metadata::Descriptor;
use parquet2::read::read_metadata;
use parquet2::schema::types::PrimitiveLogicalType::{Timestamp, Uuid};
use parquet2::schema::types::{IntegerType, PhysicalType, PrimitiveLogicalType, TimeUnit};

use crate::parquet_read::jni::{
    BOOLEAN, BYTE_ARRAY, DOUBLE, FIXED_LEN_BYTE_ARRAY, FLOAT, INT32, INT64, INT96,
};
use crate::parquet_write::schema::ColumnType;

impl ParquetDecoder {
    pub fn read(mut reader: File) -> anyhow::Result<Self> {
        let metadata = read_metadata(&mut reader)?;

        let col_count = metadata.schema().fields().len() as i32;
        let mut columns = vec![];
        let mut column_buffers = vec![];

        for f in metadata.schema_descr.columns().iter() {
            // Some types are not supported, this will skip them.
            if let Some(typ) = Self::to_column_type(&f.descriptor) {
                let physical_type = match f.descriptor.primitive_type.physical_type {
                    PhysicalType::Boolean => BOOLEAN as i64,
                    PhysicalType::Int32 => INT32 as i64,
                    PhysicalType::Int64 => INT64 as i64,
                    PhysicalType::Int96 => INT96 as i64,
                    PhysicalType::Float => FLOAT as i64,
                    PhysicalType::Double => DOUBLE as i64,
                    PhysicalType::ByteArray => BYTE_ARRAY as i64,
                    PhysicalType::FixedLenByteArray(length) => {
                        // Should match Numbers#encodeLowHighInts().
                        i64::overflowing_shl(length as i64, 32).0 | (FIXED_LEN_BYTE_ARRAY as i64)
                    }
                };

                let info = &f.descriptor.primitive_type.field_info;
                let name: Vec<u16> = info.name.encode_utf16().collect();

                columns.push(ColumnMeta {
                    typ,
                    id: info.id.unwrap_or(-1),
                    physical_type,
                    name_size: name.len() as u32,
                    name_ptr: name.as_ptr(),
                    name_vec: name,
                });

                column_buffers.push(ColumnChunkBuffers::new());
            }
        }

        // TODO: add some validation
        let decoder = ParquetDecoder {
            col_count,
            row_count: metadata.num_rows,
            row_group_count: metadata.row_groups.len() as i32,
            file: reader,
            metadata,
            decompress_buffer: vec![],
            columns_ptr: columns.as_ptr(),
            columns,
            column_buffers,
        };

        Ok(decoder)
    }

    fn to_column_type(des: &Descriptor) -> Option<ColumnType> {
        match (
            des.primitive_type.physical_type,
            des.primitive_type.logical_type,
        ) {
            (
                PhysicalType::Int64,
                Some(Timestamp {
                    unit: TimeUnit::Microseconds,
                    is_adjusted_to_utc: true,
                }),
            ) => Some(ColumnType::Timestamp),
            (
                PhysicalType::Int64,
                Some(Timestamp {
                    unit: TimeUnit::Milliseconds,
                    is_adjusted_to_utc: true,
                }),
            ) => Some(ColumnType::Date),
            (PhysicalType::Int64, None) => Some(ColumnType::Long),
            (PhysicalType::Int64, Some(PrimitiveLogicalType::Integer(IntegerType::Int64))) => {
                Some(ColumnType::Long)
            }
            (PhysicalType::Int32, None) => Some(ColumnType::Int),
            (PhysicalType::Int32, Some(PrimitiveLogicalType::Integer(IntegerType::Int32))) => {
                Some(ColumnType::Int)
            }
            (PhysicalType::Int32, Some(PrimitiveLogicalType::Integer(IntegerType::Int16))) => {
                Some(ColumnType::Short)
            }
            (PhysicalType::Int32, Some(PrimitiveLogicalType::Integer(IntegerType::Int8))) => {
                Some(ColumnType::Byte)
            }
            (PhysicalType::Boolean, None) => Some(ColumnType::Boolean),
            (PhysicalType::Double, None) => Some(ColumnType::Double),
            (PhysicalType::Float, None) => Some(ColumnType::Float),
            (PhysicalType::FixedLenByteArray(16), Some(Uuid)) => Some(ColumnType::Uuid),
            (PhysicalType::ByteArray, Some(PrimitiveLogicalType::String)) => {
                Some(ColumnType::Varchar)
            }
            (PhysicalType::FixedLenByteArray(32), None) => Some(ColumnType::Long256),
            (PhysicalType::ByteArray, None) => Some(ColumnType::Binary),
            (PhysicalType::FixedLenByteArray(16), None) => Some(ColumnType::Long128),
            (_, _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Cursor, Write};
    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::null;

    use crate::parquet_read::meta::ParquetDecoder;
    use crate::parquet_write::file::ParquetWriter;
    use arrow::datatypes::ToByteSlice;
    use bytes::Bytes;
    use tempfile::NamedTempFile;

    use crate::parquet_write::schema::{Column, ColumnType, Partition};

    #[test]
    fn test_decode_column_type_fixed() {
        let mut buf: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        let row_count = 10;
        let mut columns = Vec::new();

        let cols = vec![
            (ColumnType::Long128, size_of::<i64>() * 2, "col_long128"),
            (ColumnType::Long256, size_of::<i64>() * 4, "col_long256"),
            (ColumnType::Timestamp, size_of::<i64>(), "col_ts"),
            (ColumnType::Int, size_of::<i32>(), "col_int"),
            (ColumnType::Long, size_of::<i64>(), "col_long"),
            (ColumnType::Uuid, size_of::<i64>() * 2, "col_uuid"),
            (ColumnType::Boolean, size_of::<bool>(), "col_bool"),
            (ColumnType::Date, size_of::<i64>(), "col_date"),
            (ColumnType::Byte, size_of::<u8>(), "col_byte"),
            (ColumnType::Short, size_of::<i16>(), "col_short"),
            (ColumnType::Double, size_of::<f64>(), "col_double"),
            (ColumnType::Float, size_of::<f32>(), "col_float"),
            (ColumnType::GeoInt, size_of::<f32>(), "col_geo_int"),
            (ColumnType::GeoShort, size_of::<u16>(), "col_geo_short"),
            (ColumnType::GeoByte, size_of::<u8>(), "col_geo_byte"),
            (ColumnType::GeoLong, size_of::<i64>(), "col_geo_long"),
            (ColumnType::IPv4, size_of::<i32>(), "col_geo_ipv4"),
        ];

        for (col_type, value_size, name) in cols.iter() {
            columns.push(crate::parquet_read::meta::tests::create_fix_column(
                row_count,
                *col_type,
                *value_size,
                name,
            ));
        }

        let column_count = columns.len();
        let partition = Partition { table: "test_table".to_string(), columns };
        ParquetWriter::new(&mut buf)
            .with_statistics(false)
            .with_row_group_size(Some(1048576))
            .with_data_page_size(Some(1048576))
            .finish(partition)
            .expect("parquet writer");

        buf.set_position(0);
        let bytes: Bytes = buf.into_inner().into();
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        temp_file
            .write_all(bytes.to_byte_slice())
            .expect("Failed to write to temp file");

        let path = temp_file.path().to_str().unwrap();
        let file = File::open(Path::new(path)).unwrap();
        let meta = ParquetDecoder::read(file).unwrap();

        assert_eq!(meta.columns.len(), column_count);
        assert_eq!(meta.row_count, row_count);

        for (i, col) in meta.columns.iter().enumerate() {
            let (col_type, _, name) = cols[i];
            assert_eq!(col.typ, to_storage_type(col_type));
            let actual_name: String = String::from_utf16(&col.name_vec).unwrap();
            assert_eq!(actual_name, name);
        }

        temp_file.close().expect("Failed to delete temp file");
    }

    fn to_storage_type(column_type: ColumnType) -> ColumnType {
        match column_type {
            ColumnType::GeoInt | ColumnType::IPv4 => ColumnType::Int,
            ColumnType::GeoShort => ColumnType::Short,
            ColumnType::GeoByte => ColumnType::Byte,
            ColumnType::GeoLong => ColumnType::Long,
            other => other,
        }
    }

    fn create_fix_column(
        row_count: usize,
        col_type: ColumnType,
        value_size: usize,
        name: &'static str,
    ) -> Column {
        let mut buff = vec![0u8; row_count * value_size];
        for i in 0..row_count {
            let value = i as u8;
            let offset = i * value_size;
            buff[offset..offset + 1].copy_from_slice(&value.to_le_bytes());
        }
        let col_type_i32 = col_type as i32;
        assert_eq!(
            col_type,
            ColumnType::try_from(col_type_i32).expect("invalid colum type")
        );

        Column::from_raw_data(
            0,
            name,
            col_type as i32,
            0,
            row_count,
            buff.as_ptr(),
            buff.len(),
            null(),
            0,
            null(),
            0,
        )
        .unwrap()
    }
}
