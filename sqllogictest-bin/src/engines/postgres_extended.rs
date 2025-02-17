use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use pg_interval::Interval;
use postgres_types::Type;
use rust_decimal::Decimal;
use tokio::task::JoinHandle;

use crate::{DBConfig, Result};

pub struct PostgresExtended {
    client: Arc<tokio_postgres::Client>,
    join_handle: JoinHandle<()>,
}

impl PostgresExtended {
    pub(super) async fn connect(config: &DBConfig) -> Result<Self> {
        let (host, port) = config.random_addr();

        let (client, connection) = tokio_postgres::Config::new()
            .host(host)
            .port(port)
            .dbname(&config.db)
            .user(&config.user)
            .password(&config.pass)
            .connect(tokio_postgres::NoTls)
            .await
            .context(format!("failed to connect to postgres at {host}:{port}"))?;

        let join_handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("PostgresExtended connection error: {:?}", e);
            }
        });

        Ok(Self {
            client: Arc::new(client),
            join_handle,
        })
    }
}

impl Drop for PostgresExtended {
    fn drop(&mut self) {
        self.join_handle.abort()
    }
}

macro_rules! array_process {
    ($row:ident, $output:ident, $idx:ident, $t:ty) => {
        let value: Option<Vec<Option<$t>>> = $row.get($idx);
        match value {
            Some(value) => {
                write!($output, "{{").unwrap();
                for (i, v) in value.iter().enumerate() {
                    match v {
                        Some(v) => {
                            write!($output, "{}", v).unwrap();
                        }
                        None => {
                            write!($output, "NULL").unwrap();
                        }
                    }
                    if i < value.len() - 1 {
                        write!($output, ",").unwrap();
                    }
                }
                write!($output, "}}").unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
    ($row:ident, $output:ident, $idx:ident, $t:ty, $convert:ident) => {
        let value: Option<Vec<Option<$t>>> = $row.get($idx);
        match value {
            Some(value) => {
                write!($output, "{{").unwrap();
                for (i, v) in value.iter().enumerate() {
                    match v {
                        Some(v) => {
                            write!($output, "{}", $convert(v)).unwrap();
                        }
                        None => {
                            write!($output, "NULL").unwrap();
                        }
                    }
                    if i < value.len() - 1 {
                        write!($output, ",").unwrap();
                    }
                }
                write!($output, "}}").unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
    ($self:ident, $row:ident, $output:ident, $idx:ident, $t:ty, $ty_name:expr) => {
        let value: Option<Vec<Option<$t>>> = $row.get($idx);
        match value {
            Some(value) => {
                write!($output, "{{").unwrap();
                for (i, v) in value.iter().enumerate() {
                    match v {
                        Some(v) => {
                            let sql = format!("select ($1::{})::varchar", stringify!($ty_name));
                            let tmp_rows = $self.client.query(&sql, &[&v]).await.unwrap();
                            let value: &str = tmp_rows.get(0).unwrap().get(0);
                            assert!(value.len() > 0);
                            write!($output, "{}", value).unwrap();
                        }
                        None => {
                            write!($output, "NULL").unwrap();
                        }
                    }
                    if i < value.len() - 1 {
                        write!($output, ",").unwrap();
                    }
                }
                write!($output, "}}").unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
}

macro_rules! single_process {
    ($row:ident, $output:ident, $idx:ident, $t:ty) => {
        let value: Option<$t> = $row.get($idx);
        match value {
            Some(value) => {
                write!($output, "{}", value).unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
    ($row:ident, $output:ident, $idx:ident, $t:ty, $convert:ident) => {
        let value: Option<$t> = $row.get($idx);
        match value {
            Some(value) => {
                write!($output, "{}", $convert(&value)).unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
    ($self:ident, $row:ident, $output:ident, $idx:ident, $t:ty, $ty_name:expr) => {
        let value: Option<$t> = $row.get($idx);
        match value {
            Some(value) => {
                let sql = format!("select ($1::{})::varchar", stringify!($ty_name));
                let tmp_rows = $self.client.query(&sql, &[&value]).await.unwrap();
                let value: &str = tmp_rows.get(0).unwrap().get(0);
                assert!(value.len() > 0);
                write!($output, "{}", value).unwrap();
            }
            None => {
                write!($output, "NULL").unwrap();
            }
        }
    };
}

fn bool_to_str(value: &bool) -> &'static str {
    if *value {
        "t"
    } else {
        "f"
    }
}

fn varchar_to_str(value: &str) -> String {
    if value.is_empty() {
        "(empty)".to_string()
    } else {
        value.to_string()
    }
}

fn float4_to_str(value: &f32) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if *value == f32::INFINITY {
        "Infinity".to_string()
    } else if *value == f32::NEG_INFINITY {
        "-Infinity".to_string()
    } else {
        value.to_string()
    }
}

fn float8_to_str(value: &f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if *value == f64::INFINITY {
        "Infinity".to_string()
    } else if *value == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else {
        value.to_string()
    }
}

#[async_trait]
impl sqllogictest::AsyncDB for PostgresExtended {
    type Error = tokio_postgres::error::Error;

    async fn run(&mut self, sql: &str) -> Result<String, Self::Error> {
        use std::fmt::Write;

        let mut output = String::new();

        let is_query_sql = {
            let lower_sql = sql.to_ascii_lowercase();
            lower_sql.starts_with("select")
                || lower_sql.starts_with("values")
                || lower_sql.starts_with("show")
                || lower_sql.starts_with("with")
                || lower_sql.starts_with("describe")
        };
        if is_query_sql {
            let rows = self.client.query(sql, &[]).await?;
            for row in rows {
                for (idx, column) in row.columns().iter().enumerate() {
                    if idx != 0 {
                        write!(output, " ").unwrap();
                    }
                    match column.type_().clone() {
                        Type::INT2 => {
                            single_process!(row, output, idx, i16);
                        }
                        Type::INT4 => {
                            single_process!(row, output, idx, i32);
                        }
                        Type::INT8 => {
                            single_process!(row, output, idx, i64);
                        }
                        Type::NUMERIC => {
                            single_process!(row, output, idx, Decimal);
                        }
                        Type::DATE => {
                            single_process!(row, output, idx, NaiveDate);
                        }
                        Type::TIME => {
                            single_process!(row, output, idx, NaiveTime);
                        }
                        Type::TIMESTAMP => {
                            single_process!(row, output, idx, NaiveDateTime);
                        }
                        Type::BOOL => {
                            single_process!(row, output, idx, bool, bool_to_str);
                        }
                        Type::INT2_ARRAY => {
                            array_process!(row, output, idx, i16);
                        }
                        Type::INT4_ARRAY => {
                            array_process!(row, output, idx, i32);
                        }
                        Type::INT8_ARRAY => {
                            array_process!(row, output, idx, i64);
                        }
                        Type::BOOL_ARRAY => {
                            array_process!(row, output, idx, bool, bool_to_str);
                        }
                        Type::FLOAT4_ARRAY => {
                            array_process!(row, output, idx, f32, float4_to_str);
                        }
                        Type::FLOAT8_ARRAY => {
                            array_process!(row, output, idx, f64, float8_to_str);
                        }
                        Type::NUMERIC_ARRAY => {
                            array_process!(row, output, idx, Decimal);
                        }
                        Type::DATE_ARRAY => {
                            array_process!(row, output, idx, NaiveDate);
                        }
                        Type::TIME_ARRAY => {
                            array_process!(row, output, idx, NaiveTime);
                        }
                        Type::TIMESTAMP_ARRAY => {
                            array_process!(row, output, idx, NaiveDateTime);
                        }
                        Type::VARCHAR_ARRAY | Type::TEXT_ARRAY => {
                            array_process!(row, output, idx, String, varchar_to_str);
                        }
                        Type::VARCHAR | Type::TEXT => {
                            single_process!(row, output, idx, String, varchar_to_str);
                        }
                        Type::FLOAT4 => {
                            single_process!(row, output, idx, f32, float4_to_str);
                        }
                        Type::FLOAT8 => {
                            single_process!(row, output, idx, f64, float8_to_str);
                        }
                        Type::INTERVAL => {
                            single_process!(self, row, output, idx, Interval, INTERVAL);
                        }
                        Type::TIMESTAMPTZ => {
                            single_process!(
                                self,
                                row,
                                output,
                                idx,
                                DateTime<chrono::Utc>,
                                TIMESTAMPTZ
                            );
                        }
                        Type::INTERVAL_ARRAY => {
                            array_process!(self, row, output, idx, Interval, INTERVAL);
                        }
                        Type::TIMESTAMPTZ_ARRAY => {
                            array_process!(
                                self,
                                row,
                                output,
                                idx,
                                DateTime<chrono::Utc>,
                                TIMESTAMPTZ
                            );
                        }
                        _ => {
                            todo!("Don't support {} type now.", column.type_().name())
                        }
                    }
                }
                writeln!(output).unwrap();
            }
        } else {
            self.client.execute(sql, &[]).await?;
        }
        Ok(output)
    }

    fn engine_name(&self) -> &str {
        "postgres-extended"
    }
}
