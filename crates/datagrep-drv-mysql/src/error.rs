use datagrep_api::DbError;

pub const ER_QUERY_INTERRUPTED: u16 = 1317;

pub const ER_STATEMENT_TIMEOUT_MARIADB: u16 = 1969;
pub const ER_QUERY_TIMEOUT_MYSQL: u16 = 3024;

const ER_ACCESS_DENIED_ERROR: u16 = 1045;
const ER_DBACCESS_DENIED_ERROR: u16 = 1044;
const ER_MUST_CHANGE_PASSWORD: u16 = 1820;

pub fn map_mysql_error(err: mysql_async::Error) -> DbError {
    match err {
        mysql_async::Error::Server(e) => match e.code {
            ER_QUERY_INTERRUPTED => DbError::Cancelled,
            ER_STATEMENT_TIMEOUT_MARIADB | ER_QUERY_TIMEOUT_MYSQL => DbError::Timeout,
            ER_ACCESS_DENIED_ERROR | ER_DBACCESS_DENIED_ERROR | ER_MUST_CHANGE_PASSWORD => {
                DbError::Auth(e.message)
            }
            code => DbError::Query {
                code: Some(code.to_string()),
                message: if e.state.is_empty() || e.state == "HY000" {
                    e.message
                } else {
                    format!("{} (SQLSTATE {})", e.message, e.state)
                },
                // The MySQL protocol carries no error position offset.
                position: None,
            },
        },
        mysql_async::Error::Io(e) => DbError::Io(std::io::Error::other(e.to_string())),
        mysql_async::Error::Driver(e) => DbError::Protocol(e.to_string()),
        mysql_async::Error::Url(e) => {
            DbError::Config(datagrep_api::config::ConfigError::InvalidUrl {
                reason: e.to_string(),
            })
        }
        other => DbError::Protocol(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_err(code: u16, state: &str, message: &str) -> mysql_async::Error {
        mysql_async::Error::Server(mysql_async::ServerError {
            code,
            state: state.to_string(),
            message: message.to_string(),
        })
    }

    #[test]
    fn kill_query_maps_to_cancelled_not_failure() {
        let e = map_mysql_error(server_err(1317, "70100", "Query execution was interrupted"));
        assert!(matches!(e, DbError::Cancelled), "got {e:?}");
        assert!(
            e.is_recoverable(),
            "a killed query must not poison the conn"
        );
    }

    #[test]
    fn server_deadline_maps_to_timeout() {
        for code in [ER_STATEMENT_TIMEOUT_MARIADB, ER_QUERY_TIMEOUT_MYSQL] {
            let e = map_mysql_error(server_err(code, "HY000", "max time exceeded"));
            assert!(matches!(e, DbError::Timeout), "{code} → {e:?}");
        }
    }

    #[test]
    fn access_denied_maps_to_auth() {
        let e = map_mysql_error(server_err(1045, "28000", "Access denied for user"));
        assert!(matches!(e, DbError::Auth(_)), "got {e:?}");
    }

    #[test]
    fn plain_server_error_keeps_code_and_sqlstate() {
        let e = map_mysql_error(server_err(1146, "42S02", "Table 'x.y' doesn't exist"));
        match e {
            DbError::Query { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("1146"));
                assert!(message.contains("42S02"), "{message}");
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
