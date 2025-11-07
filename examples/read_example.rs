use anyhow::Result;
use sqlx::{SqlitePool, Row};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== SQLite Daemon Example ===\n");
    
    let db_path = Path::new("data.db");
    
    // Open read-only connection (direct access, no daemon)
    let read_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect(&format!("sqlite:{}?mode=ro", db_path.display()))
        .await?;
    
    // Set query-only mode for safety
    sqlx::query("PRAGMA query_only=ON")
        .execute(&read_pool)
        .await?;
    
    println!("📖 Reading tasks from database (direct read-only access):\n");
    
    let rows = sqlx::query("SELECT id, title, status FROM tasks")
        .fetch_all(&read_pool)
        .await?;
    
    for row in rows {
        let id: i64 = row.get(0);
        let title: String = row.get(1);
        let status: String = row.get(2);
        println!("  [{}] {} - {}", id, title, status);
    }
    
    println!("\n✓ Direct read access works!");
    println!("💡 Writes go through the daemon to serialize them.");
    
    Ok(())
}
