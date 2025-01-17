use futures::future::{join, join4};
use futures::{StreamExt, TryStreamExt};
use h2_support::prelude::*;
use h2_support::util::yield_once;

// In this case, the stream & connection both have capacity, but capacity is not
// explicitly requested.
#[tokio::test]
async fn send_data_without_requesting_capacity() {
    let _ = env_logger::try_init();

    let payload = [0; 1024];

    let mock = mock_io::Builder::new()
        .handshake()
        .write(&[
            // POST /
            0, 0, 16, 1, 4, 0, 0, 0, 1, 131, 135, 65, 139, 157, 41, 172, 75, 143, 168, 233, 25, 151,
            33, 233, 132,
        ])
        .write(&[
            // DATA
            0, 4, 0, 0, 1, 0, 0, 0, 1,
        ])
        .write(&payload[..])
        .write(frames::SETTINGS_ACK)
        // Read response
        .read(&[0, 0, 1, 1, 5, 0, 0, 0, 1, 0x89])
        .build();

    let (mut client, mut h2) = client::handshake(mock).await.unwrap();

    let request = Request::builder()
        .method(Method::POST)
        .uri("https://http2.akamai.com/")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();

    // The capacity should be immediately allocated
    assert_eq!(stream.capacity(), 0);

    // Send the data
    stream.send_data(payload[..].into(), true).unwrap();

    // Get the response
    let resp = h2.run(response).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    h2.await.unwrap();
}

#[tokio::test]
async fn release_capacity_sends_window_update() {
    let _ = env_logger::try_init();

    let payload = vec![0u8; 16_384];
    let payload_len = payload.len();

    let (io, mut srv) = mock::new();

    let mock = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.send_frame(frames::data(1, &payload[..])).await;
        srv.send_frame(frames::data(1, &payload[..])).await;
        srv.send_frame(frames::data(1, &payload[..])).await;
        srv.recv_frame(frames::window_update(0, 32_768)).await;
        srv.recv_frame(frames::window_update(1, 32_768)).await;
        srv.send_frame(frames::data(1, &payload[..]).eos()).await;
    };

    let h2 = async move {
        let (mut client, h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = async move {
            let resp = client.send_request(request, true).unwrap().0.await.unwrap();
            // Get the response
            assert_eq!(resp.status(), StatusCode::OK);
            let mut body = resp.into_parts().1;

            // read some body to use up window size to below half
            let buf = body.next().await.unwrap().unwrap();
            assert_eq!(buf.len(), payload_len);

            let buf = body.next().await.unwrap().unwrap();
            assert_eq!(buf.len(), payload_len);

            let buf = body.next().await.unwrap().unwrap();
            assert_eq!(buf.len(), payload_len);
            body.release_capacity()
                .release_capacity(buf.len() * 2)
                .unwrap();

            let buf = body.next().await.unwrap().unwrap();
            assert_eq!(buf.len(), payload_len);
        };

        join(
            async move {
                h2.await.unwrap();
            },
            req,
        )
        .await
    };
    join(mock, h2).await;
}

#[tokio::test]
async fn release_capacity_of_small_amount_does_not_send_window_update() {
    let _ = env_logger::try_init();

    let payload = [0; 16];

    let (io, mut srv) = mock::new();

    let mock = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.send_frame(frames::data(1, &payload[..]).eos()).await;
    };

    let h2 = async move {
        let (mut client, h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = async move {
            let resp = client.send_request(request, true).unwrap().0.await.unwrap();
            // Get the response
            assert_eq!(resp.status(), StatusCode::OK);
            let mut body = resp.into_parts().1;
            assert!(!body.is_end_stream());
            let buf = body.next().await.unwrap().unwrap();
            // read the small body and then release it
            assert_eq!(buf.len(), 16);
            body.release_capacity().release_capacity(buf.len()).unwrap();
            let buf = body.next().await;
            assert!(buf.is_none());
        };
        join(async move { h2.await.unwrap() }, req).await;
    };
    join(mock, h2).await;
}

#[test]
#[ignore]
fn expand_window_sends_window_update() {}

#[test]
#[ignore]
fn expand_window_calls_are_coalesced() {}

#[tokio::test]
async fn recv_data_overflows_connection_window() {
    let _ = env_logger::try_init();

    let (io, mut srv) = mock::new();

    let mock = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        // fill the whole window
        srv.send_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0u8; 16_383])).await;
        // this frame overflows the window!
        srv.send_frame(frames::data(1, vec![0u8; 128]).eos()).await;
        // expecting goaway for the conn, not stream
        srv.recv_frame(frames::go_away(0).flow_control()).await;
        // connection is ended by client
    };

    let h2 = async move {
        let (mut client, h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = async move {
            let resp = client.send_request(request, true).unwrap().0.await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.into_parts().1;
            let res = body.try_concat().await;
            let err = res.unwrap_err();
            assert_eq!(
                err.to_string(),
                "protocol error: flow-control protocol violated"
            );
        };

        // client should see a flow control error
        let conn = async move {
            let res = h2.await;
            let err = res.unwrap_err();
            assert_eq!(
                err.to_string(),
                "protocol error: flow-control protocol violated"
            );
        };
        join(conn, req).await;
    };
    join(mock, h2).await;
}

#[tokio::test]
async fn recv_data_overflows_stream_window() {
    // this tests for when streams have smaller windows than their connection
    let _ = env_logger::try_init();

    let (io, mut srv) = mock::new();

    let mock = async move {
        let _ = srv.assert_client_handshake().await;
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        // fill the whole window
        srv.send_frame(frames::data(1, vec![0u8; 16_384])).await;
        // this frame overflows the window!
        srv.send_frame(frames::data(1, &[0; 16][..]).eos()).await;
        srv.recv_frame(frames::reset(1).flow_control()).await;
    };

    let h2 = async move {
        let (mut client, conn) = client::Builder::new()
            .initial_window_size(16_384)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = async move {
            let resp = client.send_request(request, true).unwrap().0.await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.into_parts().1;
            let res = body.try_concat().await;
            let err = res.unwrap_err();
            assert_eq!(
                err.to_string(),
                "protocol error: flow-control protocol violated"
            );
        };

        join(async move { conn.await.unwrap() }, req).await;
    };
    join(mock, h2).await;
}

#[test]
#[ignore]
fn recv_window_update_causes_overflow() {
    // A received window update causes the window to overflow.
}

#[tokio::test]
async fn stream_error_release_connection_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        // we're sending the wrong content-length
        srv.send_frame(
            frames::headers(1)
                .response(200)
                .field("content-length", &*(16_384 * 3).to_string()),
        )
        .await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 10]).eos()).await;
        // mismatched content-length is a protocol error
        srv.recv_frame(frames::reset(1).protocol_error()).await;
        // but then the capacity should be released automatically
        srv.recv_frame(frames::window_update(0, 16_384 * 2 + 10))
            .await;
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = async {
            let resp = client
                .send_request(request, true)
                .unwrap()
                .0
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::OK);
            let mut body = resp.into_parts().1;
            let mut cap = body.release_capacity().clone();
            let to_release = 16_384 * 2;
            let mut should_recv_bytes = to_release;
            let mut should_recv_frames = 2usize;

            let err = body
                .try_for_each(|bytes| {
                    async move {
                        should_recv_bytes -= bytes.len();
                        should_recv_frames -= 1;
                        if should_recv_bytes == 0 {
                            assert_eq!(should_recv_bytes, 0);
                        }
                        Ok(())
                    }
                })
                .await
                .expect_err("body");
            assert_eq!(
                err.to_string(),
                "protocol error: unspecific protocol error detected"
            );
            cap.release_capacity(to_release).expect("release_capacity");
        };
        conn.drive(req).await;
        conn.await.expect("client");
    };

    join(srv, client).await;
}

#[tokio::test]
async fn stream_close_by_data_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Send request
        let (resp1, mut s1) = client.send_request(request, false).unwrap();

        // This effectively reserves the entire connection window
        s1.reserve_capacity(window_size);

        // The capacity should be immediately available as nothing else is
        // happening on the stream.
        assert_eq!(s1.capacity(), window_size);

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Create a second stream
        let (resp2, mut s2) = client.send_request(request, false).unwrap();

        // Request capacity
        s2.reserve_capacity(5);

        // There should be no available capacity (as it is being held up by
        // the previous stream
        assert_eq!(s2.capacity(), 0);

        // Closing the previous stream by sending an empty data frame will
        // release the capacity to s2
        s1.send_data("".into(), true).unwrap();

        // The capacity should be available
        assert_eq!(s2.capacity(), 5);

        // Send the frame
        s2.send_data("hello".into(), true).unwrap();

        // Drive both streams to prevent the handles from being dropped
        // (which will send a RST_STREAM) before the connection is closed.
        h2.drive(resp1).await.unwrap();
        h2.drive(resp2).await.unwrap();
    };

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.recv_frame(frames::headers(3).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::headers(3).response(200)).await;
        srv.recv_frame(frames::data(1, &b""[..]).eos()).await;
        srv.recv_frame(frames::data(3, &b"hello"[..]).eos()).await;
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn stream_close_by_trailers_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Send request
        let (resp1, mut s1) = client.send_request(request, false).unwrap();

        // This effectively reserves the entire connection window
        s1.reserve_capacity(window_size);

        // The capacity should be immediately available as nothing else is
        // happening on the stream.
        assert_eq!(s1.capacity(), window_size);

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Create a second stream
        let (resp2, mut s2) = client.send_request(request, false).unwrap();

        // Request capacity
        s2.reserve_capacity(5);

        // There should be no available capacity (as it is being held up by
        // the previous stream
        assert_eq!(s2.capacity(), 0);

        // Closing the previous stream by sending a trailers frame will
        // release the capacity to s2
        s1.send_trailers(Default::default()).unwrap();

        // The capacity should be available
        assert_eq!(s2.capacity(), 5);

        // Send the frame
        s2.send_data("hello".into(), true).unwrap();

        // Drive both streams to prevent the handles from being dropped
        // (which will send a RST_STREAM) before the connection is closed.
        h2.drive(resp1).await.unwrap();
        h2.drive(resp2).await.unwrap();
    };

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        // Get the first frame
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.recv_frame(frames::headers(3).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::headers(3).response(200)).await;
        srv.recv_frame(frames::headers(1).eos()).await;
        srv.recv_frame(frames::data(3, &b"hello"[..]).eos()).await;
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn stream_close_by_send_reset_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);

        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 16_384]).eos()).await;
        srv.recv_frame(frames::window_update(0, 16_384 * 2)).await;
        srv.recv_frame(
            frames::headers(3)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(3).response(200).eos()).await;
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.expect("client handshake");
        {
            let request = Request::builder()
                .uri("https://http2.akamai.com/")
                .body(())
                .unwrap();
            let (resp, _) = client.send_request(request, true).unwrap();
            let _res = conn.drive(resp).await;
            //    ^-- ignore the response body
        }
        let resp = {
            let request = Request::builder()
                .uri("https://http2.akamai.com/")
                .body(())
                .unwrap();
            let (resp, _) = client.send_request(request, true).unwrap();
            drop(client);
            resp
        };
        let _res = conn.drive(resp).await;
        conn.await.expect("client conn");
    };

    join(srv, client).await;
}

#[test]
#[ignore]
fn stream_close_by_recv_reset_frame_releases_capacity() {}

#[tokio::test]
async fn recv_window_update_on_stream_closed_by_data_frame() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        // Wait for the response
        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Send a data frame, this will also close the connection
        stream.send_data("hello".into(), true).unwrap();

        // keep `stream` from being dropped in order to prevent
        // it from sending an RST_STREAM frame.
        //
        // i know this is kind of evil, but it's necessary to
        // ensure that the stream is closed by the EOS frame,
        // and not by the RST_STREAM.
        std::mem::forget(stream);

        // Wait for the connection to close
        h2.await.unwrap();
    };
    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.recv_frame(frames::data(1, "hello").eos()).await;
        srv.send_frame(frames::window_update(1, 5)).await;
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn reserved_capacity_assigned_in_multi_window_updates() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        // Consume the capacity
        let payload = vec![0; frame::DEFAULT_INITIAL_WINDOW_SIZE as usize];
        stream.send_data(payload.into(), false).unwrap();

        // Reserve more data than we want
        stream.reserve_capacity(10);

        let mut stream = h2.drive(util::wait_for_capacity(stream, 5)).await;
        stream.send_data("hello".into(), false).unwrap();
        stream.send_data("world".into(), true).unwrap();

        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        h2.await.unwrap();
    };

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.recv_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0u8; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0u8; 16_383])).await;
        idle_ms(100).await;
        // Increase the connection window
        srv.send_frame(frames::window_update(0, 10)).await;
        // Incrementally increase the stream window
        srv.send_frame(frames::window_update(1, 4)).await;
        idle_ms(50).await;
        srv.send_frame(frames::window_update(1, 1)).await;
        // Receive first chunk
        srv.recv_frame(frames::data(1, "hello")).await;
        srv.send_frame(frames::window_update(1, 5)).await;
        // Receive second chunk
        srv.recv_frame(frames::data(1, "world").eos()).await;
        srv.send_frame(frames::headers(1).response(204).eos()).await;
        /*
        .recv_frame(frames::data(1, "hello").eos())
        .send_frame(frames::window_update(1, 5))
        */
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn connection_notified_on_released_capacity() {
    use futures::channel::mpsc;
    use futures::channel::oneshot;

    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    // We're going to run the connection on a thread in order to isolate task
    // notifications. This test is here, in part, to ensure that the connection
    // receives the appropriate notifications to send out window updates.

    let (tx, mut rx) = mpsc::unbounded();

    // Because threading is fun
    let (settings_tx, settings_rx) = oneshot::channel();

    let (th1_tx, th1_rx) = oneshot::channel();

    tokio::spawn(async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        settings_tx.send(()).unwrap();
        // Get the first request
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://example.com/a")
                .eos(),
        )
        .await;
        // Get the second request
        srv.recv_frame(
            frames::headers(3)
                .request("GET", "https://example.com/b")
                .eos(),
        )
        .await;
        // Send the first response
        srv.send_frame(frames::headers(1).response(200)).await;
        // Send the second response
        srv.send_frame(frames::headers(3).response(200)).await;

        // Fill the connection window
        srv.send_frame(frames::data(1, vec![0u8; 16_384]).eos())
            .await;
        idle_ms(100).await;
        srv.send_frame(frames::data(3, vec![0u8; 16_384]).eos())
            .await;

        // The window update is sent
        srv.recv_frame(frames::window_update(0, 16_384)).await;

        th1_tx.send(()).unwrap();
    });

    let (th2_tx, th2_rx) = oneshot::channel();

    let (mut client, mut h2) = client::handshake(io).await.unwrap();

    h2.drive(settings_rx).await.unwrap();
    let request = Request::get("https://example.com/a").body(()).unwrap();
    tx.unbounded_send(client.send_request(request, true).unwrap().0)
        .unwrap();

    let request = Request::get("https://example.com/b").body(()).unwrap();
    tx.unbounded_send(client.send_request(request, true).unwrap().0)
        .unwrap();

    tokio::spawn(async move {
        // Run the connection to completion
        h2.await.unwrap();

        th2_tx.send(()).unwrap();
        drop(client);
    });

    // Get the two requests
    let a = rx.next().await.unwrap();
    let b = rx.next().await.unwrap();

    // Get the first response
    let response = a.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (_, mut a) = response.into_parts();

    // Get the next chunk
    let chunk = a.next().await.unwrap();
    assert_eq!(16_384, chunk.unwrap().len());

    // Get the second response
    let response = b.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (_, mut b) = response.into_parts();

    // Get the next chunk
    let chunk = b.next().await.unwrap();
    assert_eq!(16_384, chunk.unwrap().len());

    // Wait a bit
    idle_ms(100).await;

    // Release the capacity
    a.release_capacity().release_capacity(16_384).unwrap();

    th1_rx.await.unwrap();
    th2_rx.await.unwrap();

    // Explicitly drop this after the joins so that the capacity doesn't get
    // implicitly released before.
    drop(b);
}

#[tokio::test]
async fn recv_settings_removes_available_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(0));

    let srv = async move {
        let settings = srv.assert_client_handshake_with_settings(settings).await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        idle_ms(100).await;
        srv.send_frame(frames::window_update(0, 11)).await;
        srv.send_frame(frames::window_update(1, 11)).await;
        srv.recv_frame(frames::data(1, "hello world").eos()).await;
        srv.send_frame(frames::headers(1).response(204).eos()).await;
    };

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let mut stream = h2.drive(util::wait_for_capacity(stream, 11)).await;
        assert_eq!(stream.capacity(), 11);

        stream.send_data("hello world".into(), true).unwrap();

        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        // Hold on to the `client` handle to avoid sending a GO_AWAY frame.
        h2.await.unwrap();
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn recv_settings_keeps_assigned_capacity() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let (sent_settings, sent_settings_rx) = futures::channel::oneshot::channel();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.send_frame(frames::settings().initial_window_size(64))
            .await;
        srv.recv_frame(frames::settings_ack()).await;
        sent_settings.send(()).unwrap();
        srv.recv_frame(frames::data(1, "hello world").eos()).await;
        srv.send_frame(frames::headers(1).response(204).eos()).await;
    };

    let h2 = async move {
        let (mut client, h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let f = async move {
            let mut stream = util::wait_for_capacity(stream, 11).await;
            sent_settings_rx.await.expect("rx");
            stream.send_data("hello world".into(), true).unwrap();
            let resp = response.await.expect("response");
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        };
        join(async move { h2.await.expect("h2") }, f).await;
    };

    join(srv, h2).await;
}

#[tokio::test]
async fn recv_no_init_window_then_receive_some_init_window() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(0));

    let srv = async move {
        let settings = srv.assert_client_handshake_with_settings(settings).await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        idle_ms(100).await;
        srv.send_frame(frames::settings().initial_window_size(10))
            .await;
        srv.recv_frame(frames::settings_ack()).await;
        srv.recv_frame(frames::data(1, "hello worl")).await;
        idle_ms(100).await;
        srv.send_frame(frames::settings().initial_window_size(11))
            .await;
        srv.recv_frame(frames::settings_ack()).await;
        srv.recv_frame(frames::data(1, "d").eos()).await;
        srv.send_frame(frames::headers(1).response(204).eos()).await;
    };

    let h2 = async move {
        let (mut client, mut h2) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let mut stream = h2.drive(util::wait_for_capacity(stream, 11)).await;
        assert_eq!(stream.capacity(), 11);

        stream.send_data("hello world".into(), true).unwrap();

        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        // Hold on to the `client` handle to avoid sending a GO_AWAY frame.
        h2.await.unwrap();
    };
    join(srv, h2).await;
}

#[tokio::test]
async fn settings_lowered_capacity_returns_capacity_to_connection() {
    use futures::channel::oneshot;
    use futures::future::{select, Either};
    use std::time::Instant;
    use tokio::timer::Delay;

    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();

    let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

    let (th1_tx, th1_rx) = oneshot::channel();
    // Spawn the server on a thread
    tokio::spawn(async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        tx1.send(()).unwrap();
        srv.recv_frame(frames::headers(1).request("POST", "https://example.com/one"))
            .await;
        srv.recv_frame(frames::headers(3).request("POST", "https://example.com/two"))
            .await;
        idle_ms(200).await;
        // Remove all capacity from streams
        srv.send_frame(frames::settings().initial_window_size(0))
            .await;
        srv.recv_frame(frames::settings_ack()).await;

        // Let stream 3 make progress
        srv.send_frame(frames::window_update(3, 11)).await;
        srv.recv_frame(frames::data(3, "hello world").eos()).await;
        // Wait to get notified
        //
        // A timeout is used here to avoid blocking forever if there is a
        // failure
        let result = select(rx2, Delay::new(Instant::now() + Duration::from_secs(5))).await;
        if let Either::Right((_, _)) = result {
            panic!("Timed out");
        }

        idle_ms(500).await;

        // Reset initial window size
        srv.send_frame(frames::settings().initial_window_size(window_size as u32))
            .await;
        srv.recv_frame(frames::settings_ack()).await;

        // Get data from first stream
        srv.recv_frame(frames::data(1, "hello world").eos()).await;

        // Send responses
        srv.send_frame(frames::headers(1).response(204).eos()).await;
        srv.send_frame(frames::headers(3).response(204).eos()).await;
        drop(srv);
        th1_tx.send(()).unwrap();
    });

    let (mut client, h2) = client::handshake(io).await.unwrap();

    let (th2_tx, th2_rx) = oneshot::channel();
    // Drive client connection
    tokio::spawn(async move {
        h2.await.unwrap();
        th2_tx.send(()).unwrap();
    });

    // Wait for server handshake to complete.
    let result = select(rx1, Delay::new(Instant::now() + Duration::from_secs(5))).await;
    if let Either::Right((_, _)) = result {
        panic!("Timed out");
    }

    let request = Request::post("https://example.com/one").body(()).unwrap();

    let (resp1, mut stream1) = client.send_request(request, false).unwrap();

    let request = Request::post("https://example.com/two").body(()).unwrap();

    let (resp2, mut stream2) = client.send_request(request, false).unwrap();

    // Reserve capacity for stream one, this will consume all connection level
    // capacity
    stream1.reserve_capacity(window_size);
    let stream1 = util::wait_for_capacity(stream1, window_size).await;

    // Now, wait for capacity on the other stream
    stream2.reserve_capacity(11);
    let mut stream2 = util::wait_for_capacity(stream2, 11).await;

    // Send data on stream 2
    stream2.send_data("hello world".into(), true).unwrap();

    tx2.send(()).unwrap();

    // Wait for capacity on stream 1
    let mut stream1 = util::wait_for_capacity(stream1, 11).await;

    stream1.send_data("hello world".into(), true).unwrap();

    // Wait for responses..
    let resp = resp1.await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = resp2.await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    th1_rx.await.unwrap();
    th2_rx.await.unwrap();
}

#[tokio::test]
async fn client_increase_target_window_size() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::window_update(0, (2 << 20) - 65_535))
            .await;
    };

    let client = async move {
        let (_client, mut conn) = client::handshake(io).await.unwrap();
        conn.set_target_window_size(2 << 20);
        conn.await.unwrap();
    };
    join(srv, client).await;
}

#[tokio::test]
async fn increase_target_window_size_after_using_some() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.send_frame(frames::data(1, vec![0; 16_384]).eos()).await;
        srv.recv_frame(frames::window_update(0, (2 << 20) - 65_535))
            .await;
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let res = client.send_request(request, true).unwrap().0;

        let res = conn.drive(res).await.unwrap();
        conn.set_target_window_size(2 << 20);
        // drive an empty future to allow the WINDOW_UPDATE
        // to go out while the response capacity is still in use.
        conn.drive(yield_once()).await;
        let _res = conn.drive(res.into_body().try_concat()).await;
        conn.await.expect("client");
    };

    join(srv, client).await;
}

#[tokio::test]
async fn decrease_target_window_size() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .await;
        srv.send_frame(frames::headers(1).response(200)).await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 16_384])).await;
        srv.send_frame(frames::data(1, vec![0; 16_383]).eos()).await;
        srv.recv_frame(frames::window_update(0, 16_384)).await;
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.unwrap();
        conn.set_target_window_size(16_384 * 2);

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let (resp, _) = client.send_request(request, true).unwrap();
        let res = conn.drive(resp).await.expect("response");
        conn.set_target_window_size(16_384);
        let mut body = res.into_parts().1;
        let mut cap = body.release_capacity().clone();

        let bytes = conn.drive(body.try_concat()).await.expect("concat");
        assert_eq!(bytes.len(), 65_535);
        cap.release_capacity(bytes.len()).unwrap();
        conn.await.expect("conn");
    };

    join(srv, client).await;
}

#[tokio::test]
async fn server_target_window_size() {
    let _ = env_logger::try_init();
    let (io, mut client) = mock::new();

    let client = async move {
        let settings = client.assert_server_handshake().await;
        assert_default_settings!(settings);
        client
            .recv_frame(frames::window_update(0, (2 << 20) - 65_535))
            .await;
    };
    let srv = async move {
        let mut conn = server::handshake(io).await.unwrap();
        conn.set_target_window_size(2 << 20);
        conn.next().await;
    };

    join(srv, client).await;
}

#[tokio::test]
async fn recv_settings_increase_window_size_after_using_some() {
    // See https://github.com/hyperium/h2/issues/208
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let new_win_size = 16_384 * 4; // 1 bigger than default
    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        srv.recv_frame(frames::data(1, vec![0; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16_384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16_383])).await;
        srv.send_frame(frames::settings().initial_window_size(new_win_size as u32))
            .await;
        srv.recv_frame(frames::settings_ack()).await;
        srv.send_frame(frames::window_update(0, 1)).await;
        srv.recv_frame(frames::data(1, vec![0; 1]).eos()).await;
        srv.send_frame(frames::headers(1).response(200).eos()).await;
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let (resp, mut req_body) = client.send_request(request, false).unwrap();
        req_body
            .send_data(vec![0; new_win_size].into(), true)
            .unwrap();
        let _res = conn.drive(resp).await.expect("response");
        conn.await.expect("client");
    };

    join(srv, client).await;
}

#[tokio::test]
async fn reserve_capacity_after_peer_closes() {
    // See https://github.com/hyperium/h2/issues/300
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
            .await;
        // close connection suddenly
    };

    let client = async move {
        let (mut client, mut conn) = client::handshake(io).await.unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let (resp, mut req_body) = client.send_request(request, false).unwrap();
        conn.drive(async move {
            let result = resp.await;
            assert!(result.is_err());
        })
        .await;
        // As stated in #300, this would panic because the connection
        // had already been closed.
        req_body.reserve_capacity(1);
        conn.await.expect("client");
    };

    join(srv, client).await;
}

#[tokio::test]
async fn reset_stream_waiting_for_capacity() {
    // This tests that receiving a reset on a stream that has some available
    // connection-level window reassigns that window to another stream.
    let _ = env_logger::try_init();

    let (io, mut srv) = mock::new();

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(frames::headers(1).request("GET", "http://example.com/"))
            .await;
        srv.recv_frame(frames::headers(3).request("GET", "http://example.com/"))
            .await;
        srv.recv_frame(frames::headers(5).request("GET", "http://example.com/"))
            .await;
        srv.recv_frame(frames::data(1, vec![0; 16384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16384])).await;
        srv.recv_frame(frames::data(1, vec![0; 16383]).eos()).await;
        srv.send_frame(frames::headers(1).response(200)).await;
        // Assign enough connection window for stream 3...
        srv.send_frame(frames::window_update(0, 1)).await;
        // but then reset it.
        srv.send_frame(frames::reset(3)).await;
        // 5 should use that window instead.
        srv.recv_frame(frames::data(5, vec![0; 1]).eos()).await;
        srv.send_frame(frames::headers(5).response(200)).await;
    };
    fn request() -> Request<()> {
        Request::builder()
            .uri("http://example.com/")
            .body(())
            .unwrap()
    }

    let client = async move {
        let (mut client, conn) = client::Builder::new()
            .handshake::<_, Bytes>(io)
            .await
            .expect("handshake");
        let (req1, mut send1) = client.send_request(request(), false).unwrap();
        let (req2, mut send2) = client.send_request(request(), false).unwrap();
        let (req3, mut send3) = client.send_request(request(), false).unwrap();
        // Use up the connection window.
        send1.send_data(vec![0; 65535].into(), true).unwrap();
        // Queue up for more connection window.
        send2.send_data(vec![0; 1].into(), true).unwrap();
        // .. and even more.
        send3.send_data(vec![0; 1].into(), true).unwrap();
        join4(
            async move { conn.await.expect("h2") },
            async move { req1.await.expect("req1") },
            async move { req2.await.unwrap_err() },
            async move { req3.await.expect("req3") },
        )
        .await;
    };

    join(srv, client).await;
}

#[tokio::test]
async fn data_padding() {
    let _ = env_logger::try_init();
    let (io, mut srv) = mock::new();

    let mut body = Vec::new();
    body.push(5);
    body.extend_from_slice(&[b'z'; 100][..]);
    body.extend_from_slice(&[b'0'; 5][..]);

    let srv = async move {
        let settings = srv.assert_client_handshake().await;
        assert_default_settings!(settings);
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "http://example.com/")
                .eos(),
        )
        .await;
        srv.send_frame(
            frames::headers(1)
                .response(200)
                .field("content-length", 100),
        )
        .await;
        srv.send_frame(frames::data(1, body).padded().eos()).await;
    };
    let h2 = async move {
        let (mut client, conn) = client::handshake(io).await.expect("handshake");
        let request = Request::builder()
            .method(Method::GET)
            .uri("http://example.com/")
            .body(())
            .unwrap();

        // first request is allowed
        let (response, _) = client.send_request(request, true).unwrap();
        let fut = async move {
            let resp = response.await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.into_body();
            let bytes = body.try_concat().await.unwrap();
            assert_eq!(bytes.len(), 100);
        };
        join(async move { conn.await.expect("client") }, fut).await;
    };

    join(srv, h2).await;
}
