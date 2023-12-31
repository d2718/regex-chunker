/*!
Asynchronous analogs to the base `*Chunker` types that wrap
[Tokio](https://tokio.rs/)'s
[`AsyncRead`](https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html)
types and implement
[`Stream`](https://docs.rs/futures/latest/futures/stream/trait.Stream.html).
*/

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, BytesMut};
use regex::bytes::Regex;
use tokio::io::AsyncRead;
use tokio_stream::Stream;
use tokio_util::codec::{Decoder, FramedRead};

use crate::{Adapter, MatchDisposition, RcErr};

struct ByteDecoder {
    fence: Regex,
    match_dispo: MatchDisposition,
    scan_offset: usize,
}

impl Decoder for ByteDecoder {
    type Item = Vec<u8>;
    type Error = RcErr;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let (start, end) = match self.fence.find_at(src.as_ref(), self.scan_offset) {
            Some(m) => (m.start(), m.end()),
            None => return Ok(None),
        };
        let length = end - start;

        let new_buff = match self.match_dispo {
            MatchDisposition::Drop => {
                let new_buff: Vec<u8> = src.split_to(start).into();
                src.advance(length);
                new_buff
            }
            MatchDisposition::Append => src.split_to(end).into(),
            MatchDisposition::Prepend => {
                self.scan_offset = length;
                src.split_to(start).into()
            }
        };

        Ok(Some(new_buff))
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(v) = self.decode(src)? {
            Ok(Some(v))
        } else if src.is_empty() {
            Ok(None)
        } else {
            Ok(Some(src.split().into()))
        }
    }
}

/**
The `stream::ByteChunker` is the `async` analog to the base
[`ByteChunker`](crate::ByteChunker) type. It wraps an
[`AsyncRead`](https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html)er
and implements the
[`Stream`](https://docs.rs/futures-core/0.3.28/futures_core/stream/trait.Stream.html)
trait.

This async version of the base `ByteChunker` is less flexible in how it
handles errors; you'll get errors when Tokio's underlying black magic
returns them.
*/
pub struct ByteChunker<R: AsyncRead> {
    freader: FramedRead<R, ByteDecoder>,
}

impl<R: AsyncRead> ByteChunker<R> {
    /// Return a new [`ByteChunker`] wrapping the given async reader that
    /// will chunk its output be delimiting it with the given regular
    /// expression pattern.
    pub fn new(source: R, pattern: &str) -> Result<Self, RcErr> {
        let fence = Regex::new(pattern)?;
        let decoder = ByteDecoder {
            fence,
            //error_status: ErrorStatus::Ok,
            match_dispo: MatchDisposition::default(),
            scan_offset: 0,
        };

        let freader = FramedRead::new(source, decoder);
        Ok(Self { freader })
    }

    pub fn with_adapter<A>(self, adapter: A) -> CustomChunker<R, A> {
        CustomChunker {
            chunker: self,
            adapter,
        }
    }

    /// Builder-pattern for controlling what the chunker does with the
    /// matched text; default value is [`MatchDisposition::Drop`].
    pub fn with_match(mut self, behavior: MatchDisposition) -> Self {
        let d = self.freader.decoder_mut();
        d.match_dispo = behavior;
        if matches!(behavior, MatchDisposition::Drop | MatchDisposition::Append) {
            d.scan_offset = 0;
        }
        self
    }
}

impl<A: AsyncRead + Unpin> Stream for ByteChunker<A> {
    type Item = Result<Vec<u8>, RcErr>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.freader).poll_next(cx)
    }
}

/**
The async analog to the base crate's
[`CustomChunker`](`crate::CustomChunker`).
It takes an [`Adapter`] and yields chunks based on the `Adapter`'s
transformation.

```rust
# use std::error::Error;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn Error>> {
    use regex_chunker::{
        stream::ByteChunker,
        StringAdapter,
    };
    use tokio_stream::StreamExt;
    use std::io::Cursor;

    let text = b"One, two, three four. Can I have a little more?";
    let c = Cursor::new(text);

    let chunks: Vec<_> = ByteChunker::new(c, "[ .,?]+")?
        .with_adapter(StringAdapter::default())
        .map(|res| res.unwrap())
        .collect().await;

    assert_eq!(
        &chunks,
        &[
            "One", "two", "three", "four",
            "Can", "I", "have", "a", "little", "more"
        ].clone()
    );
#   Ok(()) }
*/
pub struct CustomChunker<R: AsyncRead, A> {
    chunker: ByteChunker<R>,
    adapter: A,
}

impl<R: AsyncRead, A> CustomChunker<R, A> {
    /// Consumes the [`CustomChunker`] and returns the underlying
    /// [`ByteChunker`] and [`Adapter`].
    pub fn into_innards(self) -> (ByteChunker<R>, A) {
        (self.chunker, self.adapter)
    }

    /// Get a reference to the underlying [`Adapter`].
    pub fn get_adapter(&self) -> &A { &self.adapter }

    /// Get a mutable reference to the underlying [`Adapter`].
    pub fn get_adapter_mut(&mut self) -> &mut A { &mut self.adapter }
}

impl<R: AsyncRead, A> Unpin for CustomChunker<R, A> {}

impl<R, A> Stream for CustomChunker<R, A>
where
    R: AsyncRead + Unpin,
    A: Adapter
{
    type Item = A::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let p = Pin::new(&mut self.chunker).poll_next(cx);
        match p {
            Poll::Pending => Poll::Pending,
            Poll::Ready(x) => Poll::Ready(self.adapter.adapt(x)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::tests::{
        chunk_vec, ref_slice_cmp, HTTP_PATT, HTTP_URL, PASSWD_PATH, PASSWD_PATT, TEST_PATH,
        TEST_PATT,
    };

    use std::process::Stdio;

    use tokio::{fs::File, process::Command};
    use tokio_stream::StreamExt;

    static SOURCE: &str = "target/debug/slowsource";
    static SOURCE_ARGS: &[&str] = &[TEST_PATH, "0.0", "0.1"];

    #[tokio::test]
    async fn basic_async() {
        let byte_vec = std::fs::read(TEST_PATH).unwrap();
        let re = Regex::new(TEST_PATT).unwrap();
        let slice_vec = chunk_vec(&re, &byte_vec, MatchDisposition::Drop);

        let f = File::open(TEST_PATH).await.unwrap();
        let chunker = ByteChunker::new(f, TEST_PATT).unwrap();
        let vec_vec: Vec<Vec<u8>> = chunker.map(|res| res.unwrap()).collect().await;

        ref_slice_cmp(&vec_vec, &slice_vec);
    }

    #[tokio::test]
    async fn slow_async() {
        let byte_vec = std::fs::read(TEST_PATH).unwrap();
        let re = Regex::new(TEST_PATT).unwrap();
        let slice_vec = chunk_vec(&re, &byte_vec, MatchDisposition::Drop);

        let mut child = Command::new(SOURCE)
            .args(SOURCE_ARGS)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let chunker = ByteChunker::new(stdout, TEST_PATT).unwrap();
        let vec_vec: Vec<Vec<u8>> = chunker.map(|res| res.unwrap()).collect().await;
        child.wait().await.unwrap();

        ref_slice_cmp(&vec_vec, &slice_vec);
    }
}
