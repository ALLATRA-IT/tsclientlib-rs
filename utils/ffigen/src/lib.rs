use std::collections::HashMap;

use lazy_static::lazy_static;
use syn::*;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use quote::ToTokens;

mod csharp;
mod rust;

pub use csharp::CSharpGen;
pub use rust::RustGen;

lazy_static! {
	static ref ARRAY_KEY: RustType = RustType {
		name: String::new(),
		wrapper: None,
		content: TypeContent::Builtin(BuiltinType::Primitive(PrimitiveType::Int(false, None))),
	};
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RustType {
	/// This has no meaning for builtin types.
	/// For wrapped types, this is empty (like for builtin types) but the
	/// wrapper is set.
	pub name: String,
	pub wrapper: Option<Wrapper>,
	pub content: TypeContent,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Wrapper {
	/// The name of the type that wrapes the inner type.
	pub outer: String,
	/// The function to convert from the wrapped type to an `u64`.
	///
	/// If this is not set, `.into()` will be used.
	pub to_u64: Option<String>,
	/// The function to convert from an `u64` to the wrapped type.
	///
	/// If this is not set, `.into()` will be used.
	pub from_u64: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TypeContent {
	Struct(Struct),
	Enum(Enum),
	Builtin(BuiltinType),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Struct {
	// We cannot use a map because we need the ordering
	pub fields: Vec<(String, RustType)>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Enum {
	pub possibilities: Vec<(String, Struct)>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BuiltinType {
	/// Empty tuple `()`
	Nothing,
	Primitive(PrimitiveType),
	String,
	Str,
	Option(Box<RustType>),
	/// `Vec` or slice
	Array(Box<RustType>),
	/// `HashMap` or `BTreeMap`
	Map(Box<RustType>, Box<RustType>),
	/// `HashSet` or `BTreeSet`
	Set(Box<RustType>),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PrimitiveType {
	Bool,
	Char,
	/// `true` means signed, size is 8, 16, 32, 64, 128, None = isize/usize.
	Int(bool, Option<u8>),
	Float(u8),
}

impl From<PrimitiveType> for RustType {
	fn from(t: PrimitiveType) -> Self {
		BuiltinType::Primitive(t).into()
	}
}

impl From<BuiltinType> for RustType {
	fn from(t: BuiltinType) -> Self {
		Self {
			name: String::new(),
			wrapper: None,
			content: TypeContent::Builtin(t),
		}
	}
}

// Try to find the right type
impl<T: AsRef<str>> From<T> for RustType {
	fn from(name: T) -> Self {
		Self::from_with_wrappers(name, &HashMap::new())
	}
}

// Useful for creating enums with a single content field
impl From<RustType> for Struct {
	fn from(t: RustType) -> Self {
		Self { fields: vec![(String::new(), t)] }
	}
}

impl RustType {
	/// If this is a container type (of an array or a map), return the contained
	/// type.
	///
	/// Returns (key type, contained type)
	pub fn container_of(&self) -> Option<(&RustType, &RustType)> {
		match &self.content {
			TypeContent::Builtin(BuiltinType::Array(t)) => Some((&ARRAY_KEY, t)),
			TypeContent::Builtin(BuiltinType::Map(k, t)) => Some((k, t)),
			TypeContent::Builtin(BuiltinType::Set(t)) => Some((t, t)),
			_ => None,
		}
	}

	pub fn is_container(&self) -> bool {
		match &self.content {
			TypeContent::Builtin(BuiltinType::Option(_))
			| TypeContent::Builtin(BuiltinType::Array(_))
			| TypeContent::Builtin(BuiltinType::Map(_, _))
			| TypeContent::Builtin(BuiltinType::Set(_)) => true,
			_ => false,
		}
	}

	pub fn is_primitive(&self) -> bool {
		if let TypeContent::Builtin(BuiltinType::Primitive(_)) = self.content {
			true
		} else {
			false
		}
	}

	/// The wrapper map the name of wrapper-types to wrapped types.
	///
	/// E.g. `struct MyId(u64)` has an entry `wrappers["MyId"] = u64`.
	pub fn from_with_wrappers<S: AsRef<str>>(name: S, wrappers: &HashMap<String, RustType>) -> Self {
		let name = name.as_ref().chars().filter(|c| !c.is_whitespace()).collect::<String>();
		if let Some(t) = wrappers.get(&name) {
			return t.clone();
		}

		let first_char = name.chars().next();
		let (name, content) = match name.as_str() {
			"()" => (String::new(), TypeContent::Builtin(BuiltinType::Nothing)),
			"bool" => (String::new(), TypeContent::Builtin(BuiltinType::Primitive(PrimitiveType::Bool))),
			"char" => (String::new(), TypeContent::Builtin(BuiltinType::Primitive(PrimitiveType::Char))),
			"String" => (String::new(), TypeContent::Builtin(BuiltinType::String)),
			"str" | "&str" => (String::new(), TypeContent::Builtin(BuiltinType::Str)),
			n if n.len() >= 2
				&& (first_char.unwrap() == 'u' || first_char.unwrap() == 'i')
				&& n[1..].parse::<u8>().is_ok() => {
				(String::new(), TypeContent::Builtin(BuiltinType::Primitive(
					PrimitiveType::Int(n.chars().next().unwrap() == 'i', n[1..].parse::<u8>().ok()))))
			}
			n if n.len() >= 2
				&& first_char.unwrap() == 'f'
				&& n[1..].parse::<u8>().is_ok() => {
				(String::new(), TypeContent::Builtin(BuiltinType::Primitive(
					PrimitiveType::Float(n[1..].parse::<u8>().unwrap()))))
			}
			n if n.starts_with("Option<") => {
				let inner = &n[n.find('<').unwrap() + 1..n.len() - 1];
				(String::new(), TypeContent::Builtin(BuiltinType::Option(Box::new(
					Self::from_with_wrappers(inner.to_string(), wrappers)))))
			}
			n if n.starts_with("HashSet<") | n.starts_with("BTreeSet<") => {
				let inner = &n[n.find('<').unwrap() + 1..n.len() - 1];
				(String::new(), TypeContent::Builtin(BuiltinType::Set(Box::new(
					Self::from_with_wrappers(inner.to_string(), wrappers)))))
			}
			n if n.starts_with("Vec<") => {
				let inner = &n[n.find('<').unwrap() + 1..n.len() - 1];
				(String::new(), TypeContent::Builtin(BuiltinType::Array(Box::new(
					Self::from_with_wrappers(inner.to_string(), wrappers)))))
			}
			n if n.starts_with('[') => {
				let end = n.find(';').unwrap_or(n.len() - 1);
				let inner = &n[1..end];
				(String::new(), TypeContent::Builtin(BuiltinType::Array(Box::new(
					Self::from_with_wrappers(inner.to_string(), wrappers)))))
			}
			n if n.starts_with("HashMap<") | n.starts_with("BTreeMap<") => {
				let inner = &n[n.find('<').unwrap() + 1..n.len() - 1];
				let mut parts = inner.split(',');
				let key = parts.next().unwrap();
				let content = parts.next().unwrap();
				(String::new(), TypeContent::Builtin(BuiltinType::Map(
					Box::new(Self::from_with_wrappers(key.to_string(), wrappers)),
					Box::new(Self::from_with_wrappers(content.to_string(), wrappers)))))
			}
			_ => (name, TypeContent::Struct(Struct { fields: Vec::new() })),
		};

		Self {
			name,
			wrapper: None,
			content,
		}
	}
}

fn fields_to_struct(fields: &Fields, wrappers: &HashMap<String, RustType>) -> Struct {
	let mut struc = Struct { fields: Vec::new() };
	match fields {
		Fields::Named(n) => for f in n.named.iter() {
			let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_else(String::new);
			let ty = f.ty.clone().into_token_stream().to_string();
			struc.fields.push((n, RustType::from_with_wrappers(&ty, wrappers)));
		}
		Fields::Unnamed(n) => for f in n.unnamed.iter() {
			let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_else(String::new);
			let ty = f.ty.clone().into_token_stream().to_string();
			struc.fields.push((n, RustType::from_with_wrappers(&ty, wrappers)));
		}
		Fields::Unit => {}
	}
	struc
}

pub fn convert_struct(name: &Ident, fields: &Fields, wrappers: &HashMap<String, RustType>) -> RustType {
	RustType {
		name: name.to_string(),
		wrapper: None,
		content: TypeContent::Struct(fields_to_struct(fields, wrappers)),
	}
}

pub fn convert_enum(name: &Ident, variants: &Punctuated<Variant, Comma>, wrappers: &HashMap<String, RustType>) -> RustType {
	let mut en = Enum { possibilities: Vec::new() };
	for v in variants {
		let prefix = v.ident.to_string();
		en.possibilities.push((prefix, fields_to_struct(&v.fields, wrappers)));
	}
	RustType {
		name: name.to_string(),
		wrapper: None,
		content: TypeContent::Enum(en),
	}
}

pub fn convert_derive(input: &DeriveInput, wrappers: &HashMap<String, RustType>) -> RustType {
	match &input.data {
		Data::Struct(s) => convert_struct(&input.ident, &s.fields, wrappers),
		Data::Enum(e) => convert_enum(&input.ident, &e.variants, wrappers),
		_ => panic!("Only structs or enums are supported"),
	}
}

pub fn convert_item(input: &Item, wrappers: &HashMap<String, RustType>) -> RustType {
	match input {
		Item::Struct(s) => convert_struct(&s.ident, &s.fields, wrappers),
		Item::Enum(e) => convert_enum(&e.ident, &e.variants, wrappers),
		_ => panic!("Only structs or enums are supported"),
	}
}

/// Indent a string by a given count using tabs.
fn indent(s: &str, count: usize) -> String {
	let line_count = s.lines().count();
	let mut result = String::with_capacity(s.len() + line_count * count * 4);
	for l in s.lines() {
		if !l.is_empty() {
			result.push_str(
				std::iter::repeat("\t")
					.take(count)
					.collect::<String>()
					.as_str(),
			);
		}
		result.push_str(l);
		result.push('\n');
	}
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn simple_struct_id() {
		let t = RustType {
			name: "MyStruct".into(),
			content: TypeContent::Struct(Struct {
				fields: vec![
					("field_number_1".into(), PrimitiveType::Int(false, Some(32)).into()),
					("array".into(), BuiltinType::Array(Box::new(BuiltinType::String.into())).into()),
				],
			}),
		};
		let res = format!("{}", t);
		let split_pos = res.find("\n\n").unwrap();
		let res = &res[..split_pos];
		let res2 = "
#[derive(FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum MyStructPropertyId {
	FieldNumber1,
	ArrayLen,
	Array,
}";
		if res != res2 {
			println!("Expected result:{}", res2);
			println!("Actual result:{}", res);
		}
		assert_eq!(res, res2);
	}

	#[test]
	fn convert_u8() {
		let real: RustType = PrimitiveType::Int(false, Some(8)).into();
		assert_eq!(real, "u8".into());
	}
}
