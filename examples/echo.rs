use std::io::{Read, Result, Write};
use std::net::SocketAddr;
use std::str::FromStr;

use stuck::{net, task};

fn echo(mut stream: net::TcpStream) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    unsafe { buf.set_len(buf.capacity()) };
    loop {
        match stream.read(&mut buf)? {
            0 => break,
            n => stream.write_all(&buf[..n])?,
        }
    }
    Ok(())
}

#[stuck::main]
fn main() -> Result<()> {
    let addr = SocketAddr::from_str("127.0.0.1:0").unwrap();
    let mut listener = net::TcpListener::bind(addr)?;
    let port = listener.local_addr().unwrap().port();
    eprintln!("Listen on port: {}", port);
    let mut id_counter = 0;
    while let Ok((stream, remote_addr)) = listener.accept() {
        id_counter += 1;
        let id = id_counter;
        task::spawn(move || {
            eprintln!("{:010}[{}]: serving", id, remote_addr);
            match echo(stream) {
                Ok(_) => eprintln!("{:010}[{}]: closed", id, remote_addr),
                Err(err) => eprintln!("{:010}[{}]: {:?}", id, remote_addr, err),
            }
        });
    }
    Ok(())
}
