use crate::clvm::program::SerializedProgram;
use crate::clvm::sexp::SExp;
use crate::clvm::sexp::{AtomBuf, PairBuf};
use crate::constants::NULL_SEXP;
use crate::errors::ClvmError;
use bytes::Buf;
use dg_xch_serialize::{CONS_BOX_MARKER, MAX_SINGLE_BYTE, decode_size, encode_size};
use std::io::Read;
use std::io::{Cursor, Write};

#[derive(Debug, Copy, Clone)]
enum ParserOp {
    Exp,
    Cons,
}

#[allow(clippy::cast_possible_truncation)]
pub fn sexp_from_bytes(stream: &mut Cursor<&[u8]>) -> Result<SExp<'static>, ClvmError> {
    if !stream.has_remaining() {
        return Ok(NULL_SEXP);
    }
    let mut byte_buf = [0; 1];
    let mut op_buf = vec![ParserOp::Exp];
    let mut val_buf = vec![];
    while let Some(op) = op_buf.pop() {
        match op {
            ParserOp::Exp => {
                if !stream.has_remaining() {
                    return Err(ClvmError::UnexpectedEndOfValues(
                        "Unexpected End of SExp Stream".to_string(),
                    ));
                }
                stream.read_exact(&mut byte_buf)?;
                if byte_buf[0] == CONS_BOX_MARKER {
                    op_buf.push(ParserOp::Cons);
                    op_buf.push(ParserOp::Exp);
                    op_buf.push(ParserOp::Exp);
                } else if byte_buf[0] == 0x80 {
                    val_buf.push(NULL_SEXP);
                } else if byte_buf[0] <= MAX_SINGLE_BYTE {
                    val_buf.push(SExp::Atom(AtomBuf::new(byte_buf.to_vec())));
                } else {
                    let blob_size = decode_size(stream, byte_buf[0])?;
                    if stream.remaining() < blob_size as usize {
                        Err(ClvmError::BadEncoding)?;
                    }
                    let mut blob: Vec<u8> = vec![0; blob_size as usize];
                    stream.read_exact(&mut blob)?;
                    val_buf.push(SExp::Atom(AtomBuf::new(blob)));
                }
            }
            ParserOp::Cons => {
                if let Some(second) = val_buf.pop() {
                    if let Some(first) = val_buf.pop() {
                        val_buf.push(SExp::Pair(PairBuf::Owned((first.into(), second.into()))));
                    } else {
                        Err(ClvmError::BadEncoding)?;
                    }
                } else {
                    Err(ClvmError::BadEncoding)?;
                }
            }
        }
    }
    val_buf
        .pop()
        .ok_or_else(|| ClvmError::InvalidSyntax("Failed to Parse SExp".to_string()))
}

pub fn sexp_to_bytes(sexp: &SExp) -> std::io::Result<SerializedProgram> {
    let mut buffer = Cursor::new(Vec::new());
    let mut stack: Vec<&SExp> = vec![sexp];
    while let Some(v) = stack.pop() {
        match v {
            SExp::Atom(atom) => {
                let data = atom.as_ref();
                if data.is_empty() {
                    buffer.write_all(&[0x80_u8])?;
                } else if data.len() == 1 && (data[0] <= MAX_SINGLE_BYTE) {
                    buffer.write_all(&[data[0]])?;
                } else {
                    encode_size(&mut buffer, data.len() as u64)?;
                    buffer.write_all(data)?;
                }
            }
            SExp::Pair(pair) => {
                buffer.write_all(&[CONS_BOX_MARKER])?;
                stack.push(pair.rest());
                stack.push(pair.first());
            }
        }
    }
    Ok(buffer.into_inner().into())
}
