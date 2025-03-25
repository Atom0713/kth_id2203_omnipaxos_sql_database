use log::debug;
use omnipaxos_sql::common::kv::KVCommand;
use sqlx::{MySqlPool, Error};
use std::collections::HashMap;

pub struct Database {
    db: HashMap<String, String>,
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

        Ok(Self { pool, db: HashMap::new() })


    }

    pub async fn handle_command(&mut self, command: KVCommand) -> Option<Option<String>> {
        match command {
            KVCommand::Put(key, value) => {
                let key_clone = key.clone();
                let value_clone = value.clone();
            
                debug!("Inserting into database: key={}, value={}", key_clone, value_clone);
                let _ = sqlx::query("INSERT INTO kv (key_name, value) VALUES (?, ?)")
                    .bind(&key_clone)
                    .bind(&value_clone)
                    .execute(&self.pool)
                    .await;
                
                self.db.insert(key, value);
                None
            }
            KVCommand::Delete(key) => {
                self.db.remove(&key);
                None
            }
            KVCommand::Get(key) => Some(self.db.get(&key).map(|v| v.clone())),
        }
    }
}
