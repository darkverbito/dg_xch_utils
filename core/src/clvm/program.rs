use crate::blockchain::sized_bytes::{
    Bytes100, Bytes32, Bytes4, Bytes48, Bytes480, Bytes8, Bytes96,
};
use crate::clvm::assemble::assemble_text;
use crate::clvm::curry_utils::curry;
use crate::clvm::dialect::{ChiaDialect, NO_UNKNOWN_OPS};
use crate::clvm::parser::{sexp_from_bytes, sexp_to_bytes};
use crate::clvm::run_program::run_program;
use crate::clvm::sexp::{SExp, SExpSource};
use crate::clvm::sexp::{AtomBuf, IntoSExp};
use crate::clvm::utils::MEMPOOL_MODE;
use crate::constants::NULL_SEXP;
use crate::formatting::hex_to_bytes;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use hex::encode;
use log::error;
use num_bigint::BigInt;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::PartialEq;
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::hash::Hasher;
use std::io::{Cursor, Error, ErrorKind};
use std::path::Path;

#[derive(Eq)]
pub struct Program{
    pub serialized: SerializedProgram,
    pub sexp: SExpSource,
}

impl ChiaSerialize for Program {
    fn to_bytes(&self, _: ChiaProtocolVersion) -> Result<Vec<u8>, Error>
    where
        Self: Sized
    {
        Ok(self.serialized.buffer.as_ref().to_vec())
    }

    fn from_bytes<T: AsRef<[u8]>>(bytes: &mut Cursor<T>, _: ChiaProtocolVersion) -> Result<Self, Error>
    where
        Self: Sized
    {
        let sexp = sexp_from_bytes(bytes)?;
        let serialized = sexp_to_bytes(&sexp)?;
        Ok(Self { serialized, sexp: SExpSource::Owned(sexp) })
    }
}

impl Display for Program {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.sexp.as_ref())
    }
}

impl Debug for Program {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.sexp.as_ref())
    }
}
impl Program {
    pub fn from_sexp(sexp: SExp) -> Result<Program, Error> {
        let serial = sexp_to_bytes(&sexp)?;
        Ok(Program { serialized: serial, sexp: SExpSource::Owned(sexp) })
    }
    pub fn to<T: IntoSExp>(vals: T) -> Program {
        let sexp = vals.to_sexp();
        let serial = sexp_to_bytes(&sexp).unwrap_or_default();
        Program { serialized: serial, sexp: SExpSource::Owned(sexp) }
    }
    pub fn null() -> Self {
        let serial = sexp_to_bytes(&NULL_SEXP).unwrap_or_default();
        Program {
            serialized: serial.into(),
            sexp: SExpSource::Borrowed(&NULL_SEXP)
        }
    }
}

impl Program {
    pub fn new(serialized: SerializedProgram) -> Self {
        let mut stream = Cursor::new(&serialized);
        match sexp_from_bytes(&mut stream) {
            Ok(sexp) => Program { serialized: serialized.to_owned(), sexp: SExpSource::Owned(sexp) },
            Err(e) => {
                println!("Error building Program: {e:?}");
                Program {
                    serialized: SerializedProgram{ buffer: SerializedSource::Heap(vec![])},
                    sexp: SExpSource::Borrowed(&NULL_SEXP)
                }
            }
        }
    }
    pub fn first(&self) -> Result<Program, Error> {
        let first = self.sexp.first()?;
        let serial = sexp_to_bytes(first).unwrap_or_default();
        Ok(Program {
            serialized: serial.into(),
            sexp: SExpSource::Owned(first.clone()),
        })
    }
    pub fn rest(&self) -> Result<Program, Error> {
        let rest = self.sexp.rest()?;
        let serial = sexp_to_bytes(rest).unwrap_or_default();
        Ok(Program {
            serialized: serial.into(),
            sexp: SExpSource::Owned(rest.clone()),
        })
    }
    pub fn at(&self, path: &str) -> Result<Program, Error> {
        let mut rtn = self.sexp.as_ref();
        for c in path.chars() {
            if c == 'f' || c == 'F' {
                rtn = rtn.first()?;
            } else if c == 'r' || c == 'R' {
                rtn = rtn.rest()?;
            } else {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("`at` got illegal character `{c}`. Only `f` & `r` allowed"),
                ));
            }
        }
        let serial = sexp_to_bytes(rtn)?;
        Ok(Program {
            serialized: serial.into(),
            sexp: SExpSource::Owned(rtn.clone()),
        })
    }

    #[must_use]
    pub fn tree_hash(&self) -> Bytes32 {
        let mut stream = Cursor::new(&self.serialized);
        let sexp = sexp_from_bytes(&mut stream).unwrap_or_else(|e| {
            error!("ERROR: {e:?}");
            NULL_SEXP.clone()
        });
        sexp.tree_hash()
    }
    pub fn curry(&self, args: &[Program]) -> Result<Program, Error> {
        Ok(curry(self, args))
    }

    pub fn uncurry(&self) -> Result<(Program, Program), Error> {
        fn inner_match(o: &SExp, expected: &[u8]) -> Result<(), Error> {
            if o.atom()? == *expected {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("expected: {}", encode(expected)),
                ))
            }
        }
        {
            //(2 (1 . <mod>) <args>)
            let as_list = self.as_list();
            inner_match(&as_list[0].clone().to_sexp() /*ev*/, b"\x02")?;
            let q_pair = as_list[1].as_pair().ok_or_else(|| {
                //quoted_inner
                Error::new(
                    ErrorKind::InvalidData,
                    format!("expected pair found atom: {}", as_list[1]),
                )
            })?;
            inner_match(&q_pair.0.to_sexp(), b"\x01")?;
            let mut args = vec![];
            let mut args_list = as_list[2].clone();
            while args_list.is_pair() {
                //(4(1. < arg >) < rest >)
                let as_list = args_list.as_list();
                inner_match(&as_list[0].clone().to_sexp(), b"\x04")?;
                let q_pair = as_list[1].as_pair().ok_or_else(|| {
                    //quoted_inner
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("expected pair found atom: {}", as_list[1]),
                    )
                })?;
                inner_match(&q_pair.0.to_sexp(), b"\x01")?;
                args.push(q_pair.1.to_sexp());
                args_list = as_list[2].clone();
            }
            inner_match(&args_list.to_sexp(), b"\x01")?;
            Ok((Program::to(q_pair.1), Program::to(args)))
        }
        .or_else(|_: Error| Ok((self.clone(), Program::to(0))))
    }

    #[must_use]
    pub fn as_list(&self) -> Vec<Program> {
        match self.as_pair() {
            None => {
                vec![]
            }
            Some((first, rest)) => {
                let mut rtn: Vec<Program> = vec![first];
                rtn.extend(rest.as_list());
                rtn
            }
        }
    }

    pub fn to_map(self) -> Result<HashMap<Program, Program>, Error> {
        Ok(self
            .sexp
            .to_map()?
            .into_iter()
            .filter_map(|m| {
                if let (Ok(p1), Ok(p2)) = (sexp_to_bytes(&m.0), sexp_to_bytes(&m.1)) {
                    Some((Program::new(p1.into()), Program::new(p2.into())))
                } else {
                    None
                }
            })
            .collect())
    }

    #[must_use]
    pub fn is_atom(&self) -> bool {
        matches!(self.sexp.as_ref(), SExp::Atom(_))
    }

    #[must_use]
    pub fn is_pair(&self) -> bool {
        matches!(self.sexp.as_ref(), SExp::Pair(_))
    }

    #[must_use]
    pub fn as_atom(&self) -> Option<Program> {
        match self.sexp.as_ref() {
            SExp::Atom(_) => match sexp_to_bytes(self.sexp.as_ref()) {
                Ok(s) => Some(Program::new(s.into())),
                Err(_) => None,
            },
            SExp::Pair(_) => None,
        }
    }

    #[must_use]
    pub fn as_vec(&self) -> Option<Vec<u8>> {
        self.sexp.as_vec()
    }

    #[must_use]
    pub fn as_pair(&self) -> Option<(Program, Program)> {
        match self.sexp.as_ref() {
            SExp::Pair(pair) => {
                let left = match sexp_to_bytes(pair.first()) {
                    Ok(serial_data) => Program::new(serial_data.into()),
                    Err(_) => Program::new(Vec::new().into()),
                };
                let right = match sexp_to_bytes(pair.rest()) {
                    Ok(serial_data) => Program::new(serial_data.into()),
                    Err(_) => Program::new(Vec::new().into()),
                };
                Some((left, right))
            }
            SExp::Atom(_) => None,
        }
    }

    #[must_use]
    pub fn cons(&self, other: &Program) -> Program {
        match sexp_to_bytes(&SExp::Pair((self.sexp.as_ref(), other.sexp.as_ref()).into())) {
            Ok(bytes) => Program::new(bytes.into()),
            Err(e) => {
                println!("{e:?}");
                Program::null()
            }
        }
    }

    pub fn as_int(&self) -> Result<BigInt, Error> {
        match &self.as_atom() {
            Some(atom) => Ok(BigInt::from_signed_bytes_be(
                atom.as_vec()
                    .ok_or_else(|| {
                        Error::new(ErrorKind::InvalidData, "Failed to convert Program to Atom")
                    })?
                    .as_slice(),
            )),
            None => {
                log::debug!("BAD INT: {:?}", self.serialized);
                Err(Error::new(
                    ErrorKind::Unsupported,
                    "Program is Pair not Atom",
                ))
            }
        }
    }

    pub fn run_mempool_with_cost(
        &self,
        max_cost: u64,
        args: &Program,
    ) -> Result<(u64, Program), Error> {
        self.run(max_cost, MEMPOOL_MODE, args)
    }

    pub fn run_with_cost(&self, max_cost: u64, args: &Program) -> Result<(u64, Program), Error> {
        self.run(max_cost, 0, args)
    }

    pub fn run(&self, max_cost: u64, flags: u32, args: &Program) -> Result<(u64, Program), Error> {
        let mut stream = Cursor::new(&self.serialized);
        let program = sexp_from_bytes(&mut stream)?;
        let mut stream = Cursor::new(&args.serialized);
        let args = sexp_from_bytes(&mut stream)?;
        let dialect = ChiaDialect::new(flags | NO_UNKNOWN_OPS);
        let (cost, result) = match run_program(dialect, &program, &args, max_cost, None) {
            Ok(reduct) => reduct,
            Err(e) => {
                return Err(e);
            }
        };
        let serial = sexp_to_bytes(&result)?;
        let mut stream = Cursor::new(&serial);
        let sexp = sexp_from_bytes(&mut stream)?;
        Ok((cost, Program { serialized: serial.into(), sexp: SExpSource::Owned(sexp) }))
    }
}

impl TryFrom<Vec<u8>> for Program {
    type Error = Error;
    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        (&bytes).try_into()
    }
}

impl TryFrom<&Vec<u8>> for Program {
    type Error = Error;

    fn try_from(bytes: &Vec<u8>) -> Result<Self, Self::Error> {
        let atom = SExp::Atom(AtomBuf::from(bytes));
        Ok(Program {
            serialized: sexp_to_bytes(&atom)?.into(),
            sexp: SExpSource::Owned(atom),
        })
    }
}

impl TryFrom<&[u8]> for Program {
    type Error = Error;
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let atom = SExp::Atom(AtomBuf::from(bytes));
        Ok(Program {
            serialized: sexp_to_bytes(&atom)?.into(),
            sexp: SExpSource::Owned(atom),
        })
    }
}

impl TryFrom<(Program, Program)> for Program {
    type Error = Error;
    fn try_from((first, second): (Program, Program)) -> Result<Self, Self::Error> {
        let mut stream = Cursor::new(&first.serialized);
        let first = sexp_from_bytes(&mut stream)?;
        let mut stream = Cursor::new(&second.serialized);
        let rest = sexp_from_bytes(&mut stream)?;
        let sexp = SExp::Pair((&first, &rest).into());
        let bytes = sexp_to_bytes(&sexp)?;
        Ok(Program {
            serialized: bytes.into(),
            sexp: SExpSource::Owned(sexp),
        })
    }
}

impl Clone for Program{
    fn clone(&self) -> Program {
        Program::new(self.serialized.clone())
    }
}

impl Hash for Program {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.serialized.hash(state);
    }
}

impl PartialEq for Program {
    fn eq(&self, other: &Self) -> bool {
        self.serialized == other.serialized
    }
}

impl Default for Program {
    fn default() -> Self {
        Self {
            serialized: Default::default(),
            sexp: SExpSource::Owned(Default::default()),
        }
    }
}

macro_rules! impl_sized_bytes {
    ($($name: ident);*) => {
        $(
            impl From<$name> for Program {
                fn from(bytes: $name) -> Self {
                    Program::to(bytes)
                }
            }
        )*
    };
    ()=>{};
}

impl_sized_bytes!(
    Bytes4;
    Bytes8;
    Bytes32;
    Bytes48;
    Bytes96;
    Bytes100;
    Bytes480
);

macro_rules! impl_ints {
    ($($name: ident, $size: expr);*) => {
        $(
            impl TryFrom<$name> for Program {
                type Error = std::io::Error;
                fn try_from(int_val: $name) -> Result<Self, Self::Error> {
                    if int_val == 0 {
                        return Ok(Program::new(Vec::new().into()));
                    }
                    let as_ary = int_val.to_be_bytes();
                    let mut as_bytes = as_ary.as_slice();
                    while as_bytes.len() > 1 && as_bytes[0] == ( if as_bytes[1] & 0x80 > 0{0xFF} else {0}) {
                        as_bytes = &as_bytes[1..];
                    }
                    as_bytes.to_vec().try_into()
                }
            }
            impl TryInto<$name> for &Program {
                type Error = Error;

                fn try_into(self) -> Result<$name, Self::Error> {
                    let as_atom = self.as_vec().ok_or(Error::new(ErrorKind::InvalidInput, "Invalid program for $name"))?;
                    if as_atom.len() > $size {
                        return Err(Error::new(ErrorKind::InvalidInput, "Invalid program for $name"));
                    }
                    Ok($name::from_le_bytes(as_atom.as_slice().try_into().map_err(|e| Error::new(ErrorKind::InvalidInput, format!("Invalid program for $name: {:?}", e)))?))
                }
            }
            impl TryInto<$name> for Program {
                type Error = Error;
                fn try_into(self) -> Result<$name, Self::Error> {
                    (&self).try_into()
                }
            }
        )*
    };
    ()=>{};
}

impl_ints!(
    u8, 1;
    u16, 2;
    u32, 4;
    u64, 8;
    u128, 16;
    i8, 1;
    i16, 2;
    i32, 4;
    i64, 8;
    i128, 16
);

#[derive(Clone, PartialEq, Eq)]
pub enum SerializedSource {
    Static(&'static [u8]),
    Heap(Vec<u8>),
}
impl<'a> From<&'a SerializedSource> for Cursor<&'a [u8]> {
    fn from(value: &'a SerializedSource) -> Self {
        Cursor::new(value.as_ref())
    }
}
impl AsRef<[u8]> for SerializedSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            SerializedSource::Static(buffer) => &buffer,
            SerializedSource::Heap(buffer) => buffer.as_slice(),
        }
    }
}
impl From<Vec<u8>> for SerializedProgram {
    fn from(value: Vec<u8>) -> Self {
        Self {
            buffer: SerializedSource::Heap(value),
        }
    }
}
impl Hash for SerializedProgram {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let as_bytes: &[u8] = self.as_ref();
        as_bytes.hash(state);
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SerializedProgram {
    buffer: SerializedSource,
}
impl Default for SerializedProgram {
    fn default() -> Self {
        SerializedProgram {
            buffer: SerializedSource::Static(&[]),
        }
    }
}
impl ChiaSerialize for SerializedProgram {
    fn to_bytes(&self, _version: ChiaProtocolVersion) -> Result<Vec<u8>, Error>
    where
        Self: Sized,
    {
        let mut stream: Cursor<&[u8]> = (&self.buffer).into();
        let claim_sexp = sexp_from_bytes(&mut stream)?;
        let as_bytes = sexp_to_bytes(&claim_sexp)?;
        Ok(as_bytes.as_ref().to_vec())
    }

    fn from_bytes<T: AsRef<[u8]>>(
        bytes: &mut Cursor<T>,
        _version: ChiaProtocolVersion,
    ) -> Result<Self, Error>
    where
        Self: Sized,
    {
        let claim_sexp = sexp_from_bytes(bytes)?;
        sexp_to_bytes(&claim_sexp)
    }
}
impl Display for SerializedProgram {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", encode(&self.buffer))
    }
}
impl AsRef<[u8]> for SerializedProgram {
    fn as_ref(&self) -> &[u8] {
        match &self.buffer {
            SerializedSource::Heap(buffer) => buffer.as_slice(),
            SerializedSource::Static(buffer) => *buffer,
        }
    }
}
impl Debug for SerializedProgram {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", encode(&self.buffer))
    }
}

impl SerializedProgram {
    pub async fn from_file(path: &Path) -> Result<SerializedProgram, Error> {
        if path.ends_with("bin") {
            Ok(Self {
                buffer: SerializedSource::Heap(tokio::fs::read(path).await?),
            })
        } else if path.ends_with("hex") {
            SerializedProgram::from_hex(tokio::fs::read_to_string(&path).await?.trim())
        } else if path.ends_with("clvm") {
            assemble_text(tokio::fs::read_to_string(&path).await?.trim())
        } else {
            Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Invalid File type, Expected Hex or Bin: {path:?}"),
            ))
        }
    }
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> SerializedProgram {
        SerializedProgram {
            buffer: SerializedSource::Heap(bytes.to_owned()),
        }
    }
    pub fn from_hex(hex_str: &str) -> Result<SerializedProgram, Error> {
        Ok(SerializedProgram {
            buffer: SerializedSource::Heap(hex_to_bytes(hex_str.trim()).map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "Failed to convert str to SerializedProgram",
                )
            })?),
        })
    }
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self.buffer {
            SerializedSource::Heap(ref bytes) => bytes.clone(),
            SerializedSource::Static(ref bytes) => bytes.to_vec(),
        }
    }
    #[must_use]
    pub fn buffer(&self) -> &SerializedSource {
        &self.buffer
    }

    #[must_use]
    pub const fn const_from_bytes(bytes: &'static [u8]) -> SerializedProgram {
        SerializedProgram {
            buffer: SerializedSource::Static(bytes),
        }
    }
}

#[inline]
pub const fn strip_prefix(bytes: &[u8]) -> &[u8] {
    match bytes {
        [b'0', b'x', rest @ ..] => rest,
        _ => bytes,
    }
}

pub const fn hex_bytes_len(s: &str) -> usize {
    let b = s.as_bytes();
    let has_prefix = b.len() >= 2 && b[0] == b'0' && (b[1] | 0x20) == b'x';
    let start = if has_prefix { 2 } else { 0 };
    (b.len() - start) / 2
}

#[macro_export]
macro_rules! parse_program {
    ($hex:expr) => {
        const __STRIP: &'static [u8] = $crate::clvm::program::strip_prefix($hex.as_bytes());
        const __N: usize = __STRIP.len() / 2;
        const __ARR: [u8; __N] = match const_hex::const_decode_to_array::<__N>(__STRIP) {
            Ok(a) => a,
            Err(e) => match e {
                const_hex::FromHexError::InvalidHexCharacter { .. } => {
                    panic!("Invalid Hex Character")
                },
                const_hex::FromHexError::OddLength => {
                    panic!("Odd Length")
                },
                const_hex::FromHexError::InvalidStringLength => {
                    panic!("Invalid String Length")
                }
            },
        };
        const __AS_SEXP_BUFFER: [std::mem::MaybeUninit<$crate::clvm::sexp::SExp>; 1024] =
            $crate::clvm::parser::const_sexp_from_bytes::<1024, 1024, 1024>(__ARR.as_slice());
        const __AS_SEXP: $crate::clvm::sexp::SExp = unsafe { __AS_SEXP_BUFFER[1].assume_init_read() };
        const P2_CONDITIONS_PROGRAM: Program = $crate::clvm::program::Program {
            serialized: $crate::clvm::program::SerializedProgram {
                buffer: $crate::clvm::program::SerializedSource::Static(&__ARR),
            },
            sexp: $crate::clvm::sexp::SExpSource::Borrowed(&__AS_SEXP),
        };
    };
}

impl SerializedProgram {
    #[must_use]
    pub fn to_program(self) -> Program {
        Program::new(self)
    }
    pub fn to_owned(self) -> SerializedProgram {
        match self.buffer {
            SerializedSource::Static(s) => {
                SerializedProgram {
                    buffer: SerializedSource::Heap(s.to_vec()),
                }
            }
            SerializedSource::Heap(s) => {
                SerializedProgram {
                    buffer: SerializedSource::Heap(s),
                }
            }
        }
    }
}
impl TryFrom<String> for SerializedProgram {
    type Error = Error;

    fn try_from(hex: String) -> Result<SerializedProgram, Error> {
        SerializedProgram::from_hex(&hex)
    }
}

impl TryFrom<&str> for SerializedProgram {
    type Error = Error;

    fn try_from(hex: &str) -> Result<SerializedProgram, Error> {
        SerializedProgram::from_hex(hex)
    }
}

impl From<Program> for SerializedProgram {
    fn from(prog: Program) -> Self {
        prog.serialized
    }
}
struct SerializedProgramVisitor;

impl Visitor<'_> for SerializedProgramVisitor {
    type Value = SerializedProgram;

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("Expecting a hex String, or byte array")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        value.try_into().map_err(serde::de::Error::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        value.try_into().map_err(serde::de::Error::custom)
    }
}

impl Serialize for SerializedProgram {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'a> Deserialize<'a> for SerializedProgram {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        match deserializer.deserialize_string(SerializedProgramVisitor) {
            Ok(hex) => Ok(hex),
            Err(er) => Err(er),
        }
    }
}

struct ProgramVisitor;

impl Visitor<'_> for ProgramVisitor {
    type Value = Program;

    fn expecting(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("Expecting a hex String, or byte array")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let serial: SerializedProgram = value.try_into().map_err(serde::de::Error::custom)?;
        Ok(Program::new(serial))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let serial: SerializedProgram = value.try_into().map_err(serde::de::Error::custom)?;
        Ok(Program::new(serial))
    }
}

impl<'a> Deserialize<'a> for Program {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        match deserializer.deserialize_string(ProgramVisitor) {
            Ok(prog) => Ok(prog),
            Err(er) => Err(er),
        }
    }
}

impl Serialize for Program {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.serialized.to_string().as_str())
    }
}