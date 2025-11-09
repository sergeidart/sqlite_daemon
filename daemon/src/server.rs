use crate::router::Router;
use crate::protocol::{Request, Response};
use anyhow::Result;
use bytes::{Buf, BytesMut};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ServerOptions, NamedPipeServer};
use tracing::{debug, error, info, warn};

const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024; // 10 MB

#[cfg(windows)]
pub async fn run_server(pipe_name: &str, router: Router) -> Result<()> {
    info!(pipe_name = %pipe_name, "IPC server listening");

    let router = Arc::new(router);

    loop {
        // Create a new pipe instance for each connection
        let server = ServerOptions::new()
            .first_pipe_instance(false)  // Allow multiple instances
            .create(pipe_name)?;
        
        // Wait for a client to connect
        server.connect().await?;
        
        debug!("Client connected");
        
        // Handle this connection in a separate task
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(server, router).await {
                debug!(error = %e, "Connection handler error");
            }
        });
    }
}

#[cfg(unix)]
pub async fn run_server(pipe_name: &str, router: Router) -> Result<()> {
    use tokio::net::UnixListener;
    
    // Remove existing socket if any
    let _ = std::fs::remove_file(pipe_name);
    
    let listener = UnixListener::bind(pipe_name)?;
    info!(pipe_name = %pipe_name, "IPC server listening");

    let router = Arc::new(router);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let router = Arc::clone(&router);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection_unix(stream, router).await {
                        debug!(error = %e, "Connection handler error");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "Failed to accept connection");
            }
        }
    }
}

#[cfg(windows)]
async fn handle_connection(
    mut stream: NamedPipeServer,
    router: Arc<Router>,
) -> Result<()> {
    debug!("Client connected");

    let mut read_buf = BytesMut::with_capacity(4096);

    loop {
        // Read length prefix (4 bytes)
        while read_buf.len() < 4 {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                if read_buf.is_empty() {
                    debug!("Client disconnected");
                    return Ok(());
                } else {
                    warn!("Client disconnected mid-message");
                    return Ok(());
                }
            }
        }

        // Parse length
        let length = (&read_buf[..4]).get_u32_le() as usize;

        if length > MAX_MESSAGE_SIZE {
            error!(length = length, "Message too large");
            return Ok(()); // Close connection
        }

        // Read full message
        while read_buf.len() < 4 + length {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                warn!("Client disconnected while sending message");
                return Ok(());
            }
        }

        // Extract message
        read_buf.advance(4); // Skip length prefix
        let message_bytes = read_buf.split_to(length);

        // Parse request
        let request: Request = match serde_json::from_slice(&message_bytes) {
            Ok(req) => req,
            Err(e) => {
                error!(error = %e, "Failed to parse request");
                let response = Response::error(format!("Invalid request: {}", e));
                write_response(&mut stream, &response).await?;
                continue;
            }
        };

        debug!(request = ?request, "Received request");

        // Check if this is a shutdown request
        let is_shutdown = matches!(request, Request::Shutdown);

        // Route request to appropriate worker
        let response = router.route_request(request).await;

        // Send response
        write_response(&mut stream, &response).await?;

        // If shutdown requested, close connection
        if is_shutdown {
            debug!("Shutdown acknowledged, closing connection");
            return Ok(());
        }
    }
}

#[cfg(unix)]
async fn handle_connection_unix(
    mut stream: tokio::net::UnixStream,
    router: Arc<Router>,
) -> Result<()> {
    debug!("Client connected");

    let mut read_buf = BytesMut::with_capacity(4096);

    loop {
        // Read length prefix (4 bytes)
        while read_buf.len() < 4 {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                if read_buf.is_empty() {
                    debug!("Client disconnected");
                    return Ok(());
                } else {
                    warn!("Client disconnected mid-message");
                    return Ok(());
                }
            }
        }

        // Parse length
        let length = (&read_buf[..4]).get_u32_le() as usize;

        if length > MAX_MESSAGE_SIZE {
            error!(length = length, "Message too large");
            return Ok(()); // Close connection
        }

        // Read full message
        while read_buf.len() < 4 + length {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                warn!("Client disconnected while sending message");
                return Ok(());
            }
        }

        // Extract message
        read_buf.advance(4); // Skip length prefix
        let message_bytes = read_buf.split_to(length);

        // Parse request
        let request: Request = match serde_json::from_slice(&message_bytes) {
            Ok(req) => req,
            Err(e) => {
                error!(error = %e, "Failed to parse request");
                let response = Response::error(format!("Invalid request: {}", e));
                write_response_unix(&mut stream, &response).await?;
                continue;
            }
        };

        debug!(request = ?request, "Received request");

        // Check if this is a shutdown request
        let is_shutdown = matches!(request, Request::Shutdown);

        // Route request to appropriate worker
        let response = router.route_request(request).await;

        // Send response
        write_response_unix(&mut stream, &response).await?;

        // If shutdown requested, close connection
        if is_shutdown {
            debug!("Shutdown acknowledged, closing connection");
            return Ok(());
        }
    }
}

#[cfg(windows)]
async fn write_response(stream: &mut NamedPipeServer, response: &Response) -> Result<()> {
    let json = serde_json::to_vec(response)?;
    
    if json.len() > MAX_MESSAGE_SIZE {
        error!("Response too large");
        return Ok(()); // Just close connection
    }

    // Write length prefix
    let length = json.len() as u32;
    stream.write_all(&length.to_le_bytes()).await?;

    // Write message
    stream.write_all(&json).await?;
    stream.flush().await?;

    Ok(())
}

#[cfg(unix)]
async fn write_response_unix(stream: &mut tokio::net::UnixStream, response: &Response) -> Result<()> {
    let json = serde_json::to_vec(response)?;
    
    if json.len() > MAX_MESSAGE_SIZE {
        error!("Response too large");
        return Ok(()); // Just close connection
    }

    // Write length prefix
    let length = json.len() as u32;
    stream.write_all(&length.to_le_bytes()).await?;

    // Write message
    stream.write_all(&json).await?;
    stream.flush().await?;

    Ok(())
}
