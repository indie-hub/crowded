//! Unix transport for Doorbell command-line clients.

use std::{
    env,
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use super::protocol::{WireRequest, WireResponse};

pub(super) fn send_request(
    request: &WireRequest,
) -> Result<WireResponse, Box<dyn std::error::Error>> {
    let path = env::var_os("CROWDED_SOCKET").ok_or("CROWDED_SOCKET is not set")?;
    send_to(Path::new(&path), request)
}

fn send_to(path: &Path, request: &WireRequest) -> Result<WireResponse, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(path)?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(serde_json::from_reader(BufReader::new(stream))?)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        thread,
    };

    use super::*;
    use crate::doorbell::protocol::{RosterRequest, WireResponse};

    #[test]
    fn sends_json_line_and_reads_response() {
        let path = env::temp_dir().join(format!("crowded-client-test-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let request = WireRequest::Roster(RosterRequest {
            token: "token".into(),
            id: "request".into(),
        });
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str(&line),
                Ok(WireRequest::Roster(_))
            ));
            let mut stream = stream;
            serde_json::to_writer(&mut stream, &WireResponse::accepted("listed", None)).unwrap();
            stream.write_all(b"\n").unwrap();
        });

        assert_eq!(send_to(&path, &request).unwrap().status, "listed");
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
