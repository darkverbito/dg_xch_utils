mod tests;

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2};
use proc_macro_crate::{crate_name, FoundCrate};
use quote::{format_ident, quote};
use syn::{LitStr};

/// Public entry point:
///     parse_program_hex!("ff04ffff0101ff0280")
#[proc_macro]
pub fn parse_program_hex(input: TokenStream) -> TokenStream {
    // 1) parse the literal
    let hex = match parse_string_literal(input) {
        Ok(s) => s,
        Err(ts) => return ts,
    };

    // 2) decode hex
    let bytes = match decode_hex(&hex) {
        Ok(b) => b,
        Err(e) => return compile_error(&e),
    };

    // 3) parse CLVM -> DAG
    let dag = match parse_clvm(&bytes) {
        Ok(d) => d,
        Err(e) => return compile_error(&e),
    };

    // 4) topo order (children first)
    let order = topo_order(&dag);

    // 5) codegen
    let ts = codegen(&bytes, &dag, &order);

    ts.into()
}

/* -------------------- helpers: input parsing -------------------- */

fn parse_string_literal(input: TokenStream) -> Result<String, TokenStream> {
    match syn::parse::<LitStr>(input) {
        Ok(lit) => Ok(lit.value()),
        Err(e) => Err(e.to_compile_error().into()),
    }
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


/* -------------------- helpers: compile_error -------------------- */

fn compile_error(msg: &str) -> TokenStream {
    let ts: TokenStream2 = quote! { compile_error!(#msg); };
    ts.into()
}

/* -------------------- CLVM parsing (macro-time) -------------------- */

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
struct ParseError(&'static str);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

fn parse_clvm(stream: &[u8]) -> Result<MDag, String> {
    if stream.is_empty() {
        // constant null program
        return Ok(MDag {
            nodes: vec![MNode::Null],
            root: 0,
        });
    }
    #[derive(Copy, Clone)]
    enum Op { Exp, Cons }
    let mut nodes: Vec<MNode> = Vec::with_capacity(256);
    nodes.push(MNode::Null);
    let mut ops: Vec<Op> = vec![Op::Exp];
    let mut vals: Vec<usize> = Vec::with_capacity(256);
    let mut index = 0usize;
    while let Some(op) = ops.pop() {
        match op {
            Op::Exp => {
                ensure(stream.len() >= index + 1, "Invalid SExp: Unexpected end of stream")?;
                let cur = stream[index];
                index += 1;
                if cur == CONS_BOX_MARKER {
                    ops.push(Op::Cons);
                    ops.push(Op::Exp);
                    ops.push(Op::Exp);
                } else if cur == 0x80 {
                    vals.push(0);
                } else if cur <= MAX_SINGLE_BYTE {
                    let i = index - 1;
                    let l = 1;
                    let this = nodes.len();
                    nodes.push(MNode::Atom { i, l });
                    vals.push(this);
                } else {
                    let blob_size = decode_size(stream, &mut index, cur)
                        .map_err(|e| e.to_string())?;
                    let payload_start = index - blob_size; // (index now at end of atom)
                    let this = nodes.len();
                    nodes.push(MNode::Atom { i: payload_start, l: blob_size });
                    vals.push(this);
                }
            }
            Op::Cons => {
                ensure(vals.len() >= 2, "Bad encoding: Cons with fewer than 2 values")?;
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

fn decode_size(stream: &[u8], index: &mut usize, first: u8) -> Result<usize, ParseError> {
    if first == 0x80 {
        return Err(ParseError("internal: 0x80 encountered in decode_size"));
    }
    if first <= MAX_SINGLE_BYTE {
        return Err(ParseError("internal: <=0x7f encountered in decode_size"));
    }
    if first <= 0xbf {
        let data_len = (first & 0x7f) as usize;
        let start = *index;
        let end = start + data_len;
        if end > stream.len() {
            return Err(ParseError("Invalid SExp: bad length encoding (short)"));
        }
        *index = end;
        let total = 1 + data_len;
        Ok(total)
    } else {
        let nbytes = (first & 0x3f) as usize;
        if *index + nbytes > stream.len() {
            return Err(ParseError("Invalid SExp: length-of-length overflow"));
        }
        let mut len: usize = 0;
        for _ in 0..nbytes {
            len = (len << 8) | (stream[*index] as usize);
            *index += 1;
        }
        if *index + len > stream.len() {
            return Err(ParseError("Invalid SExp: data length overflow"));
        }
        *index += len;
        let total = 1 + nbytes + len;
        Ok(total)
    }
}

fn topo_order(dag: &MDag) -> Vec<usize> {
    let mut out = Vec::with_capacity(dag.nodes.len());
    let mut seen = vec![false; dag.nodes.len()];
    fn dfs(i: usize, dag: &MDag, seen: &mut [bool], out: &mut Vec<usize>) {
        if seen[i] { return; }
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

/* -------------------- codegen -------------------- */

fn codegen(bytes: &[u8], dag: &MDag, order: &[usize]) -> TokenStream2 {
    // create a stable private module name using a simple hash of bytes
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;
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
    quote! {
        {
            mod #mod_ident {
                pub static __ARR: [u8; #n] = [ #( #byte_literals ),* ];
                #(#node_statics)*
                pub static ROOT: &#core::clvm::sexp::SExp = &#root_ident;
            }
            // Return the Program
            #core::clvm::program::Program {
                serialized: #core::clvm::program::SerializedProgram::const_from_bytes(&#mod_ident::__ARR),
                sexp: #core::clvm::sexp::SExpSource::Borrowed(#mod_ident::ROOT),
            }
        }
    }
}

/* -------------------- hex decode -------------------- */

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);

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
