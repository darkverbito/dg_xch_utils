mod tests;

use bytes::Buf;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::io::{Cursor, Read, Seek, SeekFrom};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token};

struct Args {
    base: Ident,
    _comma: Token![,],
    hex: LitStr,
}

impl Parse for Args {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(Self {
            base: input.parse()?,   // e.g., CAT2
            _comma: input.parse()?, // ,
            hex: input.parse()?,    // "..."
        })
    }
}

/// Public entry point:
///     parse_program_hex!("ff04ffff0101ff0280")
#[proc_macro]
pub fn parse_program_hex(input: TokenStream) -> TokenStream {
    let Args { base, hex, .. } = syn::parse_macro_input!(input as Args);
    let hex = hex.value();
    let bytes = match decode_hex(hex.as_str()) {
        Ok(b) => b,
        Err(e) => return compile_error(&e),
    };
    let mut cursor = Cursor::new(bytes.as_slice());
    let dag = match parse_clvm(&mut cursor) {
        Ok(d) => d,
        Err(e) => return compile_error(&e),
    };
    let order = topo_order(&dag);
    let ts = codegen(&base, hex.as_str(), &bytes, &dag, &order);
    ts.into()
}

fn resolve_crate_path(wanted: &str) -> TokenStream2 {
    match crate_name(wanted) {
        Ok(FoundCrate::Itself) => {
            // Caller is the same crate we’re targeting (e.g. tests inside that crate)
            let ident = format_ident!("crate");
            quote!(#ident)
        }
        Ok(FoundCrate::Name(actual)) => {
            // Caller renamed the crate; use the actual name
            let ident = format_ident!("{}", actual);
            quote!(::#ident)
        }
        Err(_) => {
            // Fallback: assume the published name is usable
            let ident = format_ident!("{}", wanted);
            quote!(::#ident)
        }
    }
}

fn compile_error(msg: &str) -> TokenStream {
    let ts: TokenStream2 = quote! { compile_error!(#msg); };
    ts.into()
}

const CONS_BOX_MARKER: u8 = 0xff;
const MAX_SINGLE_BYTE: u8 = 0x7f;

/// Nodes we’ll emit as `static SExp`s
#[derive(Clone, Debug)]
enum MNode {
    Null,
    Atom { i: usize, l: usize }, // slice into shared serialized buffer
    Pair { l: usize, r: usize }, // child indices (within `nodes`)
}

#[derive(Clone, Debug)]
struct MDag {
    nodes: Vec<MNode>,
    root: usize,
}

#[derive(Debug)]
struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn parse_clvm<T: AsRef<[u8]>>(stream: &mut Cursor<T>) -> Result<MDag, String> {
    if !stream.has_remaining() {
        // constant null program
        return Ok(MDag {
            nodes: vec![MNode::Null],
            root: 0,
        });
    }
    #[derive(Copy, Clone)]
    enum Op {
        Exp,
        Cons,
    }
    let mut nodes: Vec<MNode> = Vec::with_capacity(1024);
    nodes.push(MNode::Null);
    let mut byte_buf = [0; 1];
    let mut ops: Vec<Op> = vec![Op::Exp];
    let mut vals: Vec<usize> = Vec::with_capacity(1024);
    while let Some(op) = ops.pop() {
        match op {
            Op::Exp => {
                ensure(
                    stream.has_remaining(),
                    "Invalid SExp: Unexpected end of stream",
                )?;
                stream
                    .read_exact(&mut byte_buf)
                    .map_err(|e| format!("{e:?}"))?;
                if byte_buf[0] == CONS_BOX_MARKER {
                    ops.push(Op::Cons);
                    ops.push(Op::Exp);
                    ops.push(Op::Exp);
                } else if byte_buf[0] == 0x80 {
                    vals.push(0);
                } else if byte_buf[0] <= MAX_SINGLE_BYTE {
                    let i = stream.position() as usize - 1;
                    let l = 1;
                    let this = nodes.len();
                    nodes.push(MNode::Atom { i, l });
                    vals.push(this);
                } else {
                    let blob_size =
                        decode_size(stream, byte_buf[0]).map_err(|e| e.to_string())? as usize;
                    if stream.remaining() < blob_size {
                        return Err(format!(
                            "Bad encoding: Index is {}, Length is {}, Remaining is {}",
                            stream.position(),
                            blob_size,
                            stream.remaining()
                        ));
                    }
                    let this = nodes.len();
                    nodes.push(MNode::Atom {
                        i: stream.position() as usize,
                        l: blob_size,
                    });
                    stream
                        .seek(SeekFrom::Current(blob_size as i64))
                        .map_err(|e| format!("{e:?}"))?;
                    vals.push(this);
                }
            }
            Op::Cons => {
                ensure(
                    vals.len() >= 2,
                    "Bad encoding: Cons with fewer than 2 values",
                )?;
                let r = vals.pop().unwrap();
                let l = vals.pop().unwrap();
                let this = nodes.len();
                nodes.push(MNode::Pair { l, r });
                vals.push(this);
            }
        }
    }
    ensure(vals.len() == 1, "Invalid SExp: stack not singular at end")?;
    let root = vals.pop().unwrap();
    Ok(MDag { nodes, root })
}

fn ensure(cond: bool, msg: &'static str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.into()) }
}

const MAX_DECODE_SIZE: u64 = 0x0004_0000_0000;

fn decode_size<T: AsRef<[u8]>>(stream: &mut Cursor<T>, initial_b: u8) -> Result<u64, ParseError> {
    if initial_b & 0x80 == 0 {
        return Err(ParseError("bad encoding".to_string()));
    }
    let mut bit_count = 0;
    let mut bit_mask: u8 = 0x80;
    let mut b = initial_b;
    while b & bit_mask != 0 {
        bit_count += 1;
        b &= 0xff ^ bit_mask;
        bit_mask >>= 1;
    }
    let mut size_blob: Vec<u8> = vec![0; bit_count];
    size_blob[0] = b;
    if bit_count > 1 {
        stream
            .read_exact(&mut size_blob[1..])
            .map_err(|e| ParseError(format!("{e:?}")))?;
    }
    let mut v = 0;
    if size_blob.len() > 6 {
        return Err(ParseError("bad encoding".to_string()));
    }
    for b in &size_blob {
        v <<= 8;
        v += u64::from(*b);
    }
    if v >= MAX_DECODE_SIZE {
        return Err(ParseError("bad encoding".to_string()));
    }
    Ok(v)
}

fn topo_order(dag: &MDag) -> Vec<usize> {
    let mut out = Vec::with_capacity(dag.nodes.len());
    let mut seen = vec![false; dag.nodes.len()];
    fn dfs(i: usize, dag: &MDag, seen: &mut [bool], out: &mut Vec<usize>) {
        if seen[i] {
            return;
        }
        seen[i] = true;
        match dag.nodes[i] {
            MNode::Null => {}
            MNode::Atom { .. } => {}
            MNode::Pair { l, r } => {
                dfs(l, dag, seen, out);
                dfs(r, dag, seen, out);
            }
        }
        out.push(i);
    }
    dfs(dag.root, dag, &mut seen, &mut out);
    out
}

fn codegen(base: &Ident, hex: &str, bytes: &[u8], dag: &MDag, order: &[usize]) -> TokenStream2 {
    // create a stable private module name using a simple hash of bytes
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    let hash = h.finish();
    let mod_ident = format_ident!("__clvm_prog_{hash:016x}");
    let n = bytes.len();
    let byte_literals = bytes.iter().map(|b| quote! { #b });
    // map node index -> static ident
    let mut idents = vec![format_ident!("N0"); dag.nodes.len()];
    for (slot, idx) in order.iter().enumerate() {
        // give deterministic names N1, N2... for nodes in topo order (N0 is reserved Null)
        let name = format_ident!("N{}", slot + 1);
        idents[*idx] = name;
    }
    // build `const` items for each node in topo order
    let mut node_statics: Vec<TokenStream2> = Vec::with_capacity(order.len());
    let core = resolve_crate_path("dg_xch_core");
    for &idx in order {
        let ident = &idents[idx];
        match dag.nodes[idx] {
            MNode::Null => {
                node_statics.push(quote! {
                    pub const #ident: #core::clvm::sexp::SExp = #core::constants::NULL_SEXP;
                });
            }
            MNode::Atom { i, l } => {
                if l == 0 {
                    node_statics.push(quote! {
                        pub const #ident: #core::clvm::sexp::SExp = #core::constants::NULL_SEXP;
                    });
                } else {
                    let start = i;
                    let end = i + l;
                    let elems = bytes[start..end].iter().map(|b| quote! { #b });
                    node_statics.push(quote! {
                    pub const #ident: #core::clvm::sexp::SExp =
                        #core::clvm::sexp::SExp::Atom(
                            #core::clvm::sexp::AtomBuf::Borrowed(&[#( #elems ),* ])
                        );
                    });
                }
            }
            MNode::Pair { l, r } => {
                let l_ident = &idents[l];
                let r_ident = &idents[r];
                node_statics.push(quote! {
                    pub const #ident: #core::clvm::sexp::SExp =
                        #core::clvm::sexp::SExp::Pair(
                            #core::clvm::sexp::PairBuf::Borrowed((&#l_ident, &#r_ident))
                        );
                });
            }
        }
    }
    let root_ident = &idents[dag.root];
    let ident_src = format_ident!("{}_SRC", base);
    let ident_sexp = format_ident!("{}_SEXP", base);
    let ident_program = format_ident!("{}_PROGRAM", base);
    let ident_hex = format_ident!("{}_HEX", base);
    let ident_tree_hash = format_ident!("{}_TREE_HASH", base);
    let tree_hash = compute_tree_hash(bytes, dag);
    let tree_hash_bytes = tree_hash.iter().map(|b| quote! { #b });
    quote! {
        mod #mod_ident {
            pub static __ARR: [u8; #n] = [ #( #byte_literals ),* ];
            #(#node_statics)*
            pub static ROOT: &#core::clvm::sexp::SExp = &#root_ident;
        }
        pub static #ident_src: &'static [u8] = &#mod_ident::__ARR;
        pub static #ident_sexp: &'static #core::clvm::sexp::SExp = &#mod_ident::ROOT;
        pub static #ident_program: #core::clvm::program::Program = #core::clvm::program::Program::new_static(#mod_ident::ROOT);
        pub static #ident_hex: &str = #hex;
        pub const #ident_tree_hash: #core::blockchain::sized_bytes::Bytes32 =
            #core::blockchain::sized_bytes::Bytes32::const_new([ #( #tree_hash_bytes ),* ]);
    }
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);

    if s.len() % 2 != 0 {
        return Err("Odd Length".into());
    }

    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = from_hex(bytes[i]).ok_or("Invalid Hex Character")?;
        let lo = from_hex(bytes[i + 1]).ok_or("Invalid Hex Character")?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

use sha2::{Digest, Sha256};

fn th_atom(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0x01]);
    hasher.update(data);
    hasher.finalize().into()
}

fn th_pair(l: [u8; 32], r: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0x02]);
    hasher.update(l);
    hasher.update(r);
    hasher.finalize().into()
}

fn compute_tree_hash(bytes: &[u8], dag: &MDag) -> [u8; 32] {
    // Post-order over the same topo order you already build
    fn go(idx: usize, dag: &MDag, bytes: &[u8], memo: &mut Vec<Option<[u8; 32]>>) -> [u8; 32] {
        if let Some(h) = memo[idx] {
            return h;
        }
        let h = match dag.nodes[idx] {
            MNode::Null => th_atom(&[]),
            MNode::Atom { i, l } => th_atom(&bytes[i..i + l]),
            MNode::Pair { l, r } => {
                let lh = go(l, dag, bytes, memo);
                let rh = go(r, dag, bytes, memo);
                th_pair(lh, rh)
            }
        };
        memo[idx] = Some(h);
        h
    }
    let mut memo = vec![None; dag.nodes.len()];
    go(dag.root, dag, bytes, &mut memo)
}
