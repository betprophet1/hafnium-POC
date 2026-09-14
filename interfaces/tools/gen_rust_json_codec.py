#!/usr/bin/env python3
"""Generate core/harness/src/codec.rs from interfaces/sbe/venue-journal.xml.

The generated module converts between the JSON shape the scenarios and the reference model use and SBE
bytes, through the Rust codecs sbe-tool generates, for every message in the schema:

    codec::encode(&json, &mut buf) -> Result<usize, String>
    codec::decode(&bytes) -> Result<serde_json::Value, String>

    python3 interfaces/tools/gen_rust_json_codec.py           write core/harness/src/codec.rs
    python3 interfaces/tools/gen_rust_json_codec.py --check   fail if the committed file is stale

JSON conventions: field names are the schema's; enums and set choices are their names; fixed byte arrays are
strings, zero-padded on encode and trimmed of trailing zero bytes on decode; optional fields are null when
absent; composites are nested objects; groups are arrays of objects; variable data is a string. A field
missing from input JSON encodes as zero, null, an empty array or string, or the enum's first value.
"""
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = ROOT / "interfaces" / "sbe" / "venue-journal.xml"
OUT = ROOT / "core" / "harness" / "src" / "codec.rs"
SBE_NS = "{http://fixprotocol.io/2016/sbe}"
RUST = {"uint8": "u8", "uint16": "u16", "uint32": "u32", "uint64": "u64",
        "int8": "i8", "int16": "i16", "int32": "i32", "int64": "i64", "char": "u8"}


def snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def pascal(name):
    return name[0].upper() + name[1:]


def load_types(root):
    types = {}
    for t in root.find("types"):
        name = t.get("name")
        if t.tag == "type":
            types[name] = dict(kind="prim", prim=t.get("primitiveType"), length=int(t.get("length", "1")),
                               optional=t.get("presence") == "optional")
        elif t.tag == "enum":
            types[name] = dict(kind="enum", values=[v.get("name") for v in t.findall("validValue")])
        elif t.tag == "set":
            types[name] = dict(kind="set", choices=[c.get("name") for c in t.findall("choice")])
        elif t.tag == "composite":
            types[name] = dict(kind="composite",
                               fields=[(c.get("name"), c.get("primitiveType")) for c in t.findall("type")])
    return types


class Gen:
    def __init__(self, types):
        self.types = types
        self.lines = []

    def w(self, line=""):
        self.lines.append(line)

    # ---- per-field code

    def encode_field(self, target, src, field):
        name, tname = field.get("name"), field.get("type")
        t = self.types[tname]
        m = snake(name)
        if t["kind"] == "prim" and t["length"] > 1:
            self.w(f'    {target}.{m}_zero_padded(js({src}, "{name}").as_bytes());')
        elif t["kind"] == "prim" and t["optional"]:
            conv = "as_i64" if t["prim"].startswith("int") else "as_u64"
            self.w(f'    {target}.{m}_opt({src}.get("{name}").and_then(|x| x.{conv}()).map(|x| x as {RUST[t["prim"]]}));')
        elif t["kind"] == "prim":
            getter = "ji" if t["prim"].startswith("int") else "ju"
            self.w(f'    {target}.{m}({getter}({src}, "{name}") as {RUST[t["prim"]]});')
        elif t["kind"] == "enum":
            self.w(f'    {target}.{m}(enum_{snake(tname)}({src}, "{name}")?);')
        elif t["kind"] == "set":
            self.w(f'    {target}.{m}(set_{snake(tname)}({src}, "{name}")?);')
        else:
            raise ValueError(f"unsupported field kind for {name}")

    def decode_field(self, source, into, field):
        name, tname = field.get("name"), field.get("type")
        t = self.types[tname]
        m = snake(name)
        if t["kind"] == "prim" and t["length"] > 1:
            self.w(f'    {into}.insert("{name}".into(), json!(bytes_str(&{source}.{m}())));')
        elif t["kind"] == "prim" and t["optional"]:
            self.w(f'    {into}.insert("{name}".into(), {source}.{m}().map_or(Value::Null, |x| json!(x)));')
        elif t["kind"] == "prim":
            self.w(f'    {into}.insert("{name}".into(), json!({source}.{m}()));')
        elif t["kind"] == "enum":
            self.w(f'    {into}.insert("{name}".into(), json!({source}.{m}().to_string()));')
        elif t["kind"] == "set":
            self.w(f'    {into}.insert("{name}".into(), set_json_{snake(tname)}({source}.{m}()));')
        else:
            raise ValueError(f"unsupported field kind for {name}")

    # ---- per-message code

    def message(self, msg):
        name = msg.get("name")
        mod = f"{snake(name)}_codec"
        children = list(msg)
        # encode
        self.w(f"fn encode_{snake(name)}(v: &Value, buf: &mut [u8]) -> Result<usize, String> {{")
        self.w(f"    let mut enc = {mod}::{name}Encoder::default().wrap(WriteBuf::new(buf), message_header_codec::ENCODED_LENGTH);")
        self.w("    enc = enc.header(0).parent().map_err(sbe_err)?;")
        for c in children:
            if c.tag != "field":
                continue
            t = self.types[c.get("type")]
            if t["kind"] == "composite":
                cname = c.get("name")
                self.w(f'    let cv = &v["{cname}"];')
                self.w(f"    let mut c = enc.{snake(cname)}_encoder();")
                for fname, prim in t["fields"]:
                    getter = "ji" if prim.startswith("int") else "ju"
                    self.w(f'    c.{snake(fname)}({getter}(cv, "{fname}") as {RUST[prim]});')
                self.w("    enc = c.parent().map_err(sbe_err)?;")
            else:
                self.encode_field("enc", "v", c)
        for c in children:
            if c.tag == "group":
                gname = c.get("name")
                self.w(f'    let items = ja(v, "{gname}");')
                self.w(f"    let mut g = enc.{snake(gname)}_encoder(items.len() as u16, {mod}::encoder::{pascal(gname)}Encoder::default());")
                self.w("    for item in items {")
                self.w("        g.advance().map_err(sbe_err)?;")
                for f in c.findall("field"):
                    self.encode_field("    g", "item", f)
                self.w("    }")
                self.w("    enc = g.parent().map_err(sbe_err)?;")
        for c in children:
            if c.tag == "data":
                self.w(f'    enc.{snake(c.get("name"))}(js(v, "{c.get("name")}").as_bytes());')
        self.w("    Ok(enc.get_limit())")
        self.w("}")
        self.w()
        # decode
        self.w(f"fn decode_{snake(name)}<'a>(header: MessageHeaderDecoder<ReadBuf<'a>>, buf: &'a [u8]) -> Result<Value, String> {{")
        self.w("    let mut m = Map::new();")
        self.w(f'    m.insert("msg".into(), json!("{name}"));')
        self.w(f"    let mut dec = {mod}::{name}Decoder::default().header(header, 0);")
        for c in children:
            if c.tag != "field":
                continue
            t = self.types[c.get("type")]
            if t["kind"] == "composite":
                cname = c.get("name")
                self.w(f"    let mut c = dec.{snake(cname)}_decoder();")
                self.w("    let mut cm = Map::new();")
                for fname, _ in t["fields"]:
                    self.w(f'    cm.insert("{fname}".into(), json!(c.{snake(fname)}()));')
                self.w("    dec = c.parent().map_err(sbe_err)?;")
                self.w(f'    m.insert("{cname}".into(), Value::Object(cm));')
            else:
                self.decode_field("dec", "m", c)
        for c in children:
            if c.tag == "group":
                gname = c.get("name")
                self.w(f"    let mut g = dec.{snake(gname)}_decoder();")
                self.w("    let mut arr = Vec::new();")
                self.w("    while g.advance().map_err(sbe_err)?.is_some() {")
                self.w("        let mut im = Map::new();")
                for f in c.findall("field"):
                    self.decode_field("    g", "im", f)
                self.w("        arr.push(Value::Object(im));")
                self.w("    }")
                self.w("    dec = g.parent().map_err(sbe_err)?;")
                self.w(f'    m.insert("{gname}".into(), Value::Array(arr));')
        for c in children:
            if c.tag == "data":
                dname = c.get("name")
                self.w(f"    let (offset, len) = dec.{snake(dname)}_decoder();")
                self.w(f'    m.insert("{dname}".into(), json!(String::from_utf8_lossy(&buf[offset..offset + len])));')
        self.w("    let _ = &mut dec;")
        self.w("    Ok(Value::Object(m))")
        self.w("}")
        self.w()

    def enum_helpers(self, used):
        # sbe-tool generates modules only for types some message uses, so helpers follow the same rule.
        for tname, t in sorted(self.types.items()):
            if tname not in used:
                continue
            mod = snake(tname)
            if t["kind"] == "enum":
                self.w(f"fn enum_{mod}(v: &Value, k: &str) -> Result<{mod}::{tname}, String> {{")
                self.w("    match js(v, k) {")
                self.w(f'        "" => Ok({mod}::{tname}::{t["values"][0]}),')
                for val in t["values"]:
                    self.w(f'        "{val}" => Ok({mod}::{tname}::{val}),')
                self.w(f'        other => Err(format!("{{k}}: {{other}} is not a {tname}")),')
                self.w("    }")
                self.w("}")
                self.w()
            elif t["kind"] == "set":
                self.w(f"fn set_{mod}(v: &Value, k: &str) -> Result<{mod}::{tname}, String> {{")
                self.w(f"    let mut s = {mod}::{tname}::new(0);")
                self.w("    for choice in ja(v, k) {")
                self.w("        match choice.as_str().unwrap_or(\"\") {")
                for ch in t["choices"]:
                    self.w(f'            "{ch}" => {{ s.set_{ch.lower()}(true); }}')
                self.w(f'            other => return Err(format!("{{k}}: {{other}} is not a {tname} choice")),')
                self.w("        }")
                self.w("    }")
                self.w("    Ok(s)")
                self.w("}")
                self.w()
                self.w(f"fn set_json_{mod}(s: {mod}::{tname}) -> Value {{")
                self.w("    let mut out = Vec::new();")
                for ch in t["choices"]:
                    self.w(f'    if s.get_{ch.lower()}() {{ out.push(json!("{ch}")); }}')
                self.w("    Value::Array(out)")
                self.w("}")
                self.w()


def generate():
    root = ET.parse(SCHEMA).getroot()
    types = load_types(root)
    messages = [m for m in root if m.tag == SBE_NS + "message"]
    g = Gen(types)
    g.w("// @generated by interfaces/tools/gen_rust_json_codec.py from interfaces/sbe/venue-journal.xml.")
    g.w("// Do not edit: change the schema or the generator and regenerate.")
    g.w("#![allow(clippy::all, clippy::pedantic, unused_mut, unused_variables)]")
    g.w()
    g.w("use serde_json::{json, Map, Value};")
    g.w("use venue_journal::message_header_codec::MessageHeaderDecoder;")
    g.w("use venue_journal::*;")
    g.w()
    g.w("fn sbe_err(e: SbeErr) -> String { format!(\"sbe: {e}\") }")
    g.w("fn ju(v: &Value, k: &str) -> u64 { v.get(k).and_then(Value::as_u64).unwrap_or(0) }")
    g.w("fn ji(v: &Value, k: &str) -> i64 { v.get(k).and_then(Value::as_i64).unwrap_or(0) }")
    g.w("fn js<'a>(v: &'a Value, k: &str) -> &'a str { v.get(k).and_then(Value::as_str).unwrap_or(\"\") }")
    g.w("fn ja<'a>(v: &'a Value, k: &str) -> &'a [Value] { v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice) }")
    g.w("fn bytes_str(b: &[u8]) -> String {")
    g.w("    let end = b.iter().rposition(|x| *x != 0).map_or(0, |i| i + 1);")
    g.w("    String::from_utf8_lossy(&b[..end]).into_owned()")
    g.w("}")
    g.w()
    g.w("/// Encodes a message given as JSON into `buf`, returning the encoded length.")
    g.w("pub fn encode(v: &Value, buf: &mut [u8]) -> Result<usize, String> {")
    g.w('    match v.get("msg").and_then(Value::as_str) {')
    for m in messages:
        g.w(f'        Some("{m.get("name")}") => encode_{snake(m.get("name"))}(v, buf),')
    g.w('        other => Err(format!("unknown message {other:?}")),')
    g.w("    }")
    g.w("}")
    g.w()
    g.w("/// Decodes one SBE message into JSON.")
    g.w("pub fn decode(buf: &[u8]) -> Result<Value, String> {")
    g.w("    let header = MessageHeaderDecoder::default().wrap(ReadBuf::new(buf), 0);")
    g.w("    if header.schema_id() != SBE_SCHEMA_ID {")
    g.w('        return Err(format!("schema {} is not {}", header.schema_id(), SBE_SCHEMA_ID));')
    g.w("    }")
    g.w("    match header.template_id() {")
    for m in messages:
        g.w(f"        {snake(m.get('name'))}_codec::SBE_TEMPLATE_ID => decode_{snake(m.get('name'))}(header, buf),")
    g.w('        other => Err(format!("unknown template {other}")),')
    g.w("    }")
    g.w("}")
    g.w()
    used = {f.get("type") for m in messages for f in m.iter("field")}
    g.enum_helpers(used)
    for m in messages:
        g.message(m)
    return "\n".join(g.lines).rstrip() + "\n"


def main(argv):
    text = generate()
    if argv == ["--check"]:
        if not OUT.exists() or OUT.read_text() != text:
            print(f"{OUT.relative_to(ROOT)} is stale: run python3 interfaces/tools/gen_rust_json_codec.py")
            return 1
        print(f"{OUT.relative_to(ROOT)} is current")
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(ROOT)} ({text.count(chr(10))} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
