use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

fn transmit(mut socket: std::os::unix::net::UnixStream, size: usize, seed: usize) {
    let data: Vec<u8> = (0..size).map(|i| ((i * 31 + seed) % 251) as u8).collect();
    socket.write_all(&data).unwrap();
}
fn receive(socket: &mut std::os::unix::net::UnixStream, size: usize, seed: usize, slow: bool) {
    let mut buffer = [0; 16384];
    let mut offset = 0;
    let mut checksum = 0u64;
    while offset < size {
        let want = buffer.len().min(size - offset);
        let count = socket.read(&mut buffer[..want]).unwrap();
        assert_ne!(count, 0, "EOF offset={offset} wanted={size}");
        for (i, &byte) in buffer[..count].iter().enumerate() {
            assert_eq!(byte, (((offset + i) * 31 + seed) % 251) as u8);
            checksum += u64::from(byte);
        }
        offset += count;
        if slow {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    eprintln!("LARGE_READ bytes={size} checksum={checksum}");
}

#[test]
#[ignore = "requires KVM and external artifacts; select parameters with LARGE_* environment"]
fn mapped_large_streams() {
    let parameter =
        |name: &str, default: usize| std::env::var(name).map_or(default, |s| s.parse().unwrap());
    let size = parameter("LARGE_SIZE", 1024 * 1024);
    let direction = parameter("LARGE_DIRECTION", 0);
    let streams = parameter("LARGE_STREAMS", 1);
    let slow = parameter("LARGE_SLOW", 0) != 0;
    let path = std::env::temp_dir().join(format!("embed-large-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    let peer = std::thread::spawn(move || {
        let mut workers = Vec::new();
        while workers.len() < streams + 1 {
            match listener.accept() {
                Ok((mut socket, _)) => workers.push(std::thread::spawn(move || {
                    let timeout = Some(Duration::from_secs(35));
                    socket.set_read_timeout(timeout).unwrap();
                    socket.set_write_timeout(timeout).unwrap();
                    let mut seed = [0; 4];
                    socket.read_exact(&mut seed).unwrap();
                    let seed = u32::from_ne_bytes(seed) as usize;
                    if seed == 0 {
                        for i in 0..4096usize {
                            let mut byte = [0];
                            socket.read_exact(&mut byte).unwrap();
                            assert_eq!(byte, [i as u8]);
                            socket.write_all(&byte).unwrap();
                        }
                        let mut ack = [0];
                        socket.read_exact(&mut ack).unwrap();
                        assert_eq!(ack, [b'!']);
                        eprintln!("LARGE_SMALL_OK messages=4096");
                        return;
                    }
                    let writer = (direction == 2).then(|| {
                        let socket = socket.try_clone().unwrap();
                        std::thread::spawn(move || transmit(socket, size, seed + 17))
                    });
                    if direction == 1 {
                        transmit(socket.try_clone().unwrap(), size, seed + 17);
                    } else {
                        receive(&mut socket, size, seed, slow);
                    }
                    if let Some(writer) = writer {
                        writer.join().unwrap();
                    }
                    for i in 0..128u8 {
                        let mut byte = [0];
                        socket.read_exact(&mut byte).unwrap();
                        assert_eq!(byte, [i]);
                        socket.write_all(&byte).unwrap();
                    }
                    let mut ack = [0];
                    socket.read_exact(&mut ack).unwrap();
                    assert_eq!(ack, [b'!']);
                    eprintln!(
                        "LARGE_STREAM_OK bytes={size} direction={direction} elapsed={:.6}",
                        start.elapsed().as_secs_f64()
                    );
                })),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < Duration::from_secs(40), "accept timeout");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let mut builder = krun::Builder::new();
    builder
        .internet_socket_forwarding(false)
        .add_vsock_port(7000, path.clone(), false);
    let mode = format!(
        "embed_large_{size}_{direction}_{streams}_{}",
        usize::from(slow)
    );
    assert!(super::run(builder, &mode).contains("EMBED_LARGE_OK"));
    peer.join().unwrap();
    std::fs::remove_file(path).unwrap();
    eprintln!("LARGE_RESULT bytes={size} direction={direction} streams={streams} slow={slow} seconds={:.6}", start.elapsed().as_secs_f64());
}
