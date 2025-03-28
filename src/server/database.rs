use log::debug;
use omnipaxos_sql::common::kv::SQLCommand;
use sqlx::{MySqlPool, Error, Row};

pub struct Database {
    pool: MySqlPool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self, Error> {
        debug!("Connecting to database with URL: {}", database_url);
        let pool = MySqlPool::connect(database_url).await?;

        debug!("Creating table if it doesn't exist");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS kv (
                id SERIAL PRIMARY KEY,
                key_name VARCHAR(255) NOT NULL UNIQUE,
                value TEXT NOT NULL
            )"
        )
        .execute(&pool)
        .await?;

        debug!("Clearing the kv table");
        sqlx::query("TRUNCATE TABLE kv")
            .execute(&pool)
            .await?;

        Ok(Self { pool})


    }

    pub async fn handle_command(&mut self, command: SQLCommand) -> Option<Option<String>> {
        match command {
            SQLCommand::Insert(key, query) => {
                debug!("Check if key: {} exists", key);
                let exists_query = format!("SELECT COUNT(*) FROM kv WHERE key_name = '{}'", key);
                let count: i64 = sqlx::query_scalar(&exists_query).fetch_one(&self.pool).await.unwrap();

                if count > 0 {
                    debug!("Duplicate entry detected for key: {}, skipping insert", key);
                    return None;
                }
            
                debug!("Insert key: {key}");
                sqlx::query(&query).execute(&self.pool).await.unwrap();
                None
            }
            SQLCommand::Select(_, key, query) => {
                debug!("Read key: {}", key);
                let rows: Vec<sqlx::mysql::MySqlRow> = match sqlx::query(&query).fetch_all(&self.pool).await {
                    Ok(rows) => rows, // Unwrap the Result to get the Vec<MySqlRow>
                    Err(e) => {
                        debug!("Error executing query: {}", e);
                        return Some(None); // Return None if there is an error
                    }
                };
            
            if rows.is_empty() {
                debug!("No rows returned for key: {}", key);
                return Some(None); // Return `None` if no rows are found
            }

            // Convert rows to a single string
            let result_string = rows
                .iter()
                .map(|row| row.try_get::<String, _>("value").unwrap_or_default()) // Call `try_get` on each row
                .collect::<Vec<_>>()
                .join(", ");

            Some(Some(result_string))
            }
            SQLCommand::Delete(key, query) => {
                debug!("Delete key: {}", key);
                let _ = sqlx::query(&query).execute(&self.pool).await;
                None
            }
        }
    }
}
