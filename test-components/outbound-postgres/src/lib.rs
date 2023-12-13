use helper::{ensure, ensure_eq, ensure_matches, ensure_ok};

use bindings::fermyon::spin2_0_0::{postgres, rdbms_types};

helper::define_component!(Component);
const DB_URL_ENV: &str = "DB_URL";

impl Component {
    fn main() -> Result<(), String> {
        ensure_matches!(
            postgres::Connection::open("hello"),
            Err(postgres::Error::ConnectionFailed(_))
        );
        ensure_matches!(
            postgres::Connection::open("localhost:10000"),
            Err(postgres::Error::ConnectionFailed(_))
        );

        let address = ensure_ok!(std::env::var(DB_URL_ENV));
        let conn = ensure_ok!(postgres::Connection::open(&address));

        let rowset = ensure_ok!(numeric_types(&conn));
        ensure!(rowset.rows.iter().all(|r| r.len() == 12));
        ensure_matches!(rowset.rows[0][11], rdbms_types::DbValue::Floating64(f) if f == 1.0);

        let rowset = ensure_ok!(character_types(&conn));
        ensure!(rowset.rows.iter().all(|r| r.len() == 3));
        ensure!(matches!(rowset.rows[0][0], rdbms_types::DbValue::Str(ref s) if s == "rvarchar"));

        let rowset = ensure_ok!(nullable(&conn));
        ensure!(rowset.rows.iter().all(|r| r.len() == 1));
        ensure!(matches!(rowset.rows[0][0], rdbms_types::DbValue::DbNull));

        let pid1 = format!("{:?}", ensure_ok!(pg_backend_pid(&conn)));
        let pid2 = format!("{:?}", ensure_ok!(pg_backend_pid(&conn)));
        ensure_eq!(pid1, pid2);

        Ok(())
    }
}

fn numeric_types(conn: &postgres::Connection) -> Result<postgres::RowSet, postgres::Error> {
    let create_table_sql = r#"
        CREATE TEMPORARY TABLE test_numeric_types (
            intid integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
            rsmallserial smallserial NOT NULL,
            rsmallint smallint NOT NULL,
            rint2 int2 NOT NULL,
            rserial serial NOT NULL,
            rint int NOT NULL,
            rint4 int4 NOT NULL,
            rbigserial bigserial NOT NULL,
            rbigint bigint NOT NULL,
            rint8 int8 NOT NULL,
            rreal real NOT NULL,
            rdouble double precision NOT NULL
         );
    "#;

    conn.execute(create_table_sql, &[])?;

    let insert_sql = r#"
        INSERT INTO test_numeric_types
            (rsmallint, rint2, rint, rint4, rbigint, rint8, rreal, rdouble)
        VALUES
            (0, 0, 0, 0, 0, 0, 0, 1);
    "#;

    conn.execute(insert_sql, &[])?;

    let sql = r#"
        SELECT
            intid,
            rsmallserial,
            rsmallint,
            rint2,
            rserial,
            rint,
            rint4,
            rbigserial,
            rbigint,
            rint8,
            rreal,
            rdouble
        FROM test_numeric_types;
    "#;

    conn.query(sql, &[])
}

fn character_types(conn: &postgres::Connection) -> Result<postgres::RowSet, postgres::Error> {
    let create_table_sql = r#"
        CREATE TEMPORARY TABLE test_character_types (
            rvarchar varchar(40) NOT NULL,
            rtext text NOT NULL,
            rchar char(10) NOT NULL
         );
    "#;

    conn.execute(create_table_sql, &[])?;

    let insert_sql = r#"
        INSERT INTO test_character_types
            (rvarchar, rtext, rchar)
        VALUES
            ('rvarchar', 'rtext', 'rchar');
    "#;

    conn.execute(insert_sql, &[])?;

    let sql = r#"
        SELECT
            rvarchar, rtext, rchar
        FROM test_character_types;
    "#;

    conn.query(sql, &[])
}

fn nullable(conn: &postgres::Connection) -> Result<postgres::RowSet, postgres::Error> {
    let create_table_sql = r#"
        CREATE TEMPORARY TABLE test_nullable (
            rvarchar varchar(40)
         );
    "#;

    conn.execute(create_table_sql, &[])?;

    let insert_sql = r#"
        INSERT INTO test_nullable
            (rvarchar)
        VALUES
            ($1);
    "#;

    conn.execute(insert_sql, &[rdbms_types::ParameterValue::DbNull])?;

    let sql = r#"
        SELECT
            rvarchar
        FROM test_nullable;
    "#;

    conn.query(sql, &[])
}

fn pg_backend_pid(conn: &postgres::Connection) -> Result<rdbms_types::DbValue, postgres::Error> {
    let sql = "SELECT pg_backend_pid()";

    let rowset = conn.query(sql, &[])?;

    Ok(rowset.rows[0][0].clone())
}