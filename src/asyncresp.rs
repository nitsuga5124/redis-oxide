use std::convert::From;
use std::io;
use std::str;

use crate::types::{RedisValue, NULL_ARRAY, NULL_BULK_STRING};

use bytes::BytesMut;
use std::net::AddrParseError;
use tokio::codec::{Decoder, Encoder};

use combine;
use combine::byte::{byte, crlf, take_until_bytes};

use combine::combinator::{any_send_partial_state, AnySendPartialState};
#[allow(unused_imports)] // See https://github.com/rust-lang/rust/issues/43970
use combine::error::StreamError;
use combine::parser::choice::choice;
use combine::range::{recognize, take};
use combine::stream::{FullRangeStream, StreamErrorFor};

// Dummy tuple struct to impl Extend/Default on
struct ResultExtend<T, E>(Result<T, E>);

impl<T, E> Default for ResultExtend<T, E>
where
    T: Default,
{
    fn default() -> Self {
        ResultExtend(Ok(T::default()))
    }
}

/// Trait to allow us to grab more bytes if we're still Ok::<U>,
/// Or just keep an error and set self to it.
impl<T, U, E> Extend<Result<U, E>> for ResultExtend<T, E>
where
    T: Extend<U>,
{
    fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = Result<U, E>>,
    {
        let mut returned_err = None;
        if let Ok(ref mut elems) = self.0 {
            elems.extend(iter.into_iter().scan((), |_, item| match item {
                Ok(item) => Some(item),
                Err(err) => {
                    returned_err = Some(err);
                    None
                }
            }))
        }
        if let Some(err) = returned_err {
            self.0 = Err(err);
        }
    }
}

parser! {
   type PartialState = AnySendPartialState;
   fn redis_parser['a, I]()(I) -> Result<RedisValue, String>
    where [I: FullRangeStream<Item = u8, Range = &'a [u8]> ] {
       let word = || recognize(take_until_bytes(&b"\r\n"[..]).with(take(2).map(|_| ())));

       let simple_string = || word().map(|word: &[u8]| {
           RedisValue::SimpleString(word[..word.len() - 2].to_vec()) // TODO: Don't have to index like this
       });

       let error = || word().map(|word: &[u8]| {
           RedisValue::Error(word[..word.len() - 2].to_vec()) // TODO: Don't have to index like this
       });

       let int = || word().and_then(|word| {
           let word = str::from_utf8(&word[..word.len() - 2])
               .map_err(StreamErrorFor::<I>::other)?;
           match word.trim().parse::<i64>() {
               Err(_) => Err(StreamErrorFor::<I>::message_static_message("Expected integer, got garbage")),
               Ok(value) => Ok(value),
           }
       });

       let bulk_string = || int().then_partial(move |length| {
           if *length < 0 {
               combine::value(RedisValue::NullBulkString).left()
           } else {
               take(*length as usize)
                   .map(|s: &[u8]| RedisValue::BulkString(s.to_vec()))
                   .skip(crlf())
                   .right()
           }
       });

       let array = || int().then_partial(move |length| {
           if *length < 0 {
               combine::value(RedisValue::NullArray).map(Ok).left()
           } else {
               let length = *length as usize;
               combine::count_min_max(length, length, redis_parser())
                   .map(|result: ResultExtend<_, _>| {
                       result.0.map(RedisValue::Array)
                   }).right()
           }
       });

       any_send_partial_state(choice((
           byte(b'+').with(simple_string().map(Ok)),
           byte(b':').with(int().map(RedisValue::Int).map(Ok)),
           byte(b'-').with(error().map(Ok)),
           byte(b'$').with(bulk_string().map(Ok)),
           byte(b'*').with(array()),
       )))
    }
}

#[derive(Debug)]
pub enum R02Error {
    IOError(std::io::Error),
    AddrParseError(String),
    Else(String),
}

impl From<std::net::AddrParseError> for R02Error {
    fn from(err: AddrParseError) -> R02Error {
        R02Error::AddrParseError(err.to_string())
    }
}

impl From<String> for R02Error {
    fn from(err: String) -> R02Error {
        R02Error::Else(err)
    }
}

impl From<R02Error> for std::io::Error {
    fn from(err: R02Error) -> std::io::Error {
        if let R02Error::IOError(e) = err {
            return e;
        }
        // TODO: Not do this, or even have this impl
        std::io::Error::new(std::io::ErrorKind::InvalidData, "IO Error")
    }
}

impl From<io::Error> for R02Error {
    fn from(err: io::Error) -> R02Error {
        R02Error::IOError(err)
    }
}

#[derive(Default)]
pub struct RedisValueCodec {
    state: AnySendPartialState,
}

impl Decoder for RedisValueCodec {
    type Item = RedisValue;
    type Error = R02Error;
    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let (opt, removed_len) = {
            let buffer = &bytes[..];
            let stream = combine::easy::Stream(combine::stream::PartialStream(buffer));
            match combine::stream::decode(redis_parser(), stream, &mut self.state) {
                Ok(x) => x,
                Err(err) => {
                    let err = err
                        .map_position(|pos| pos.translate_position(buffer))
                        .map_range(|range| format!("{:?}", range))
                        .to_string();
                    return Err(R02Error::Else(err));
                }
            }
        };

        bytes.split_to(removed_len);

        match opt {
            Some(result) => Ok(Some(result?)),
            None => Ok(None),
        }
    }
}

fn write_redis_value(item: RedisValue, dst: &mut BytesMut) {
    match item {
        RedisValue::Error(e) => {
            dst.extend_from_slice(b"-");
            dst.extend_from_slice(&e);
            dst.extend_from_slice(b"\r\n");
        }
        RedisValue::SimpleString(s) => {
            dst.extend_from_slice(b"+");
            dst.extend_from_slice(&s);
            dst.extend_from_slice(b"\r\n");
        }
        RedisValue::BulkString(s) => {
            dst.extend_from_slice(b"$");
            dst.extend_from_slice(s.len().to_string().as_bytes());
            dst.extend_from_slice(b"\r\n");
            dst.extend_from_slice(&s);
            dst.extend_from_slice(b"\r\n");
        }
        RedisValue::Array(array) => {
            dst.extend_from_slice(b"*");
            dst.extend_from_slice(array.len().to_string().as_bytes());
            dst.extend_from_slice(b"\r\n");
            for redis_value in array {
                write_redis_value(redis_value, dst);
            }
        }
        RedisValue::Int(i) => {
            dst.extend_from_slice(b":");
            dst.extend_from_slice(i.to_string().as_bytes());
            dst.extend_from_slice(b"\r\n");
        }
        RedisValue::NullArray => dst.extend_from_slice(NULL_ARRAY.as_bytes()),
        RedisValue::NullBulkString => dst.extend_from_slice(NULL_BULK_STRING.as_bytes()),
    }
}

impl Encoder for RedisValueCodec {
    type Item = RedisValue;
    type Error = io::Error;

    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> io::Result<()> {
        write_redis_value(item, dst);
        Ok(())
    }
}

#[cfg(test)]
mod async_resp_tests {
    use crate::asyncresp::RedisValueCodec;
    use crate::types::{RedisValue, Value};
    use bytes::BytesMut;
    use pretty_assertions::assert_eq;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tokio::codec::{Decoder, Encoder};

    proptest! {
        #[test]
        fn proptest_no_crash_utf8(input: String) {
            let mut decoder = RedisValueCodec::default();
            // Only care that it doesn't crash.
            let _ = decoder.decode(&mut BytesMut::from(input.clone()));
       }
        #[test]
        fn proptest_no_crash_non_utf8(input in vec(any::<u8>(), 255)) {
            let first: usize = input.len() / 2;
            let second = input.len() - first;
            let mut seq = vec![
                BytesMut::from(&input[0..first]),
                BytesMut::from(&input[second..]),
            ];

            // Only care that it doesn't crash.
            let mut decoder = RedisValueCodec::default();
            let _ = decoder.decode(&mut seq[0]);
            let _ = decoder.decode(&mut seq[1]);
        }

    }

    fn generic_test(input: &'static str, output: RedisValue) {
        let mut decoder = RedisValueCodec::default();
        let result_read = decoder.decode(&mut BytesMut::from(input));

        let mut encoder = RedisValueCodec::default();
        let mut buf = BytesMut::new();
        let result_write = encoder.encode(output.clone(), &mut buf);

        assert!(
            result_write.as_ref().is_ok(),
            "{:?}",
            result_write.unwrap_err()
        );

        assert_eq!(input.clone().as_bytes(), buf.as_ref());

        assert!(
            result_read.as_ref().is_ok(),
            "{:?}",
            result_read.unwrap_err()
        );
        let values = result_read.unwrap().unwrap();

        let generic_arr_test_case = vec![output.clone(), output.clone()];
        let doubled = input.to_owned() + &input.to_owned();

        assert_eq!(output, values);
        generic_test_arr(&doubled, generic_arr_test_case)
    }

    fn generic_test_arr(input: &str, output: Vec<RedisValue>) {
        // TODO: Try to make this occur randomly
        let first: usize = input.len() / 2;
        let second = input.len() - first;
        let mut seq = vec![
            BytesMut::from(&input[0..first]),
            BytesMut::from(&input[second..]),
        ];

        let mut decoder = RedisValueCodec::default();
        let mut res = Vec::new();
        loop {
            match decoder.decode(&mut seq[0]) {
                Ok(Some(value)) => {
                    res.push(value);
                }
                Err(e) => panic!("Should not error, {:?}", e),
                _ => break,
            }
        }
        loop {
            match decoder.decode(&mut seq[1]) {
                Ok(Some(value)) => {
                    res.push(value);
                }
                Err(e) => panic!("Should not error, {:?}", e),
                _ => break,
            }
        }
        assert_eq!(output, res);
    }

    fn ezs() -> Value {
        "hello".as_bytes().to_vec()
    }

    #[test]
    fn test_simple_string() {
        let t = RedisValue::SimpleString(ezs());
        let s = "+hello\r\n";
        generic_test(s, t);

        let t = RedisValue::SimpleString(ezs());
        let s = "+hello\r\n+hello\r\n";
        generic_test_arr(s, vec![t.clone(), t.clone()]);
    }

    #[test]
    fn test_error() {
        let t = RedisValue::Error(ezs());
        let s = "-hello\r\n";
        generic_test(s, t);

        let t = RedisValue::Error(ezs());
        let s = "-hello\r\n-hello\r\n";
        generic_test_arr(s, vec![t.clone(), t.clone()]);
    }

    #[test]
    fn test_array() {
        let t = RedisValue::Array(vec![]);
        let s = "*0\r\n";
        generic_test(s, t);

        let inner = vec![
            RedisValue::BulkString("foo".as_bytes().to_vec()),
            RedisValue::BulkString("bar".as_bytes().to_vec()),
        ];
        let t = RedisValue::Array(inner);
        let s = "*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        generic_test(s, t);

        let inner = vec![RedisValue::Int(1), RedisValue::Int(2), RedisValue::Int(3)];
        let t = RedisValue::Array(inner);
        let s = "*3\r\n:1\r\n:2\r\n:3\r\n";
        generic_test(s, t);

        let inner = vec![
            RedisValue::Int(1),
            RedisValue::Int(2),
            RedisValue::Int(3),
            RedisValue::Int(4),
            RedisValue::BulkString("foobar".as_bytes().to_vec()),
        ];
        let t = RedisValue::Array(inner);
        let s = "*5\r\n:1\r\n:2\r\n:3\r\n:4\r\n$6\r\nfoobar\r\n";
        generic_test(s, t);

        let inner = vec![
            RedisValue::Array(vec![
                RedisValue::Int(1),
                RedisValue::Int(2),
                RedisValue::Int(3),
            ]),
            RedisValue::Array(vec![
                RedisValue::SimpleString("Foo".as_bytes().to_vec()),
                RedisValue::Error("Bar".as_bytes().to_vec()),
            ]),
        ];
        let t = RedisValue::Array(inner);
        let s = "*2\r\n*3\r\n:1\r\n:2\r\n:3\r\n*2\r\n+Foo\r\n-Bar\r\n";
        generic_test(s, t);

        let inner = vec![
            RedisValue::BulkString("foo".as_bytes().to_vec()),
            RedisValue::NullBulkString,
            RedisValue::BulkString("bar".as_bytes().to_vec()),
        ];
        let t = RedisValue::Array(inner);
        let s = "*3\r\n$3\r\nfoo\r\n$-1\r\n$3\r\nbar\r\n";
        generic_test(s, t);

        let t = RedisValue::NullArray;
        let s = "*-1\r\n";
        generic_test(s, t);
    }

    #[test]
    fn test_bulk_string() {
        let t = RedisValue::BulkString(ezs());
        let s = "$5\r\nhello\r\n";
        generic_test(s, t);

        let t = RedisValue::BulkString("".as_bytes().to_vec());
        let s = "$0\r\n\r\n";
        generic_test(s, t);
    }
}
