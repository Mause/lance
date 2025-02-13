// Copyright 2023 Lance Developers.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Lance Schema Field

use std::{cmp::max, fmt, sync::Arc};

use arrow_array::{
    cast::AsArray,
    types::{
        Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
    },
    ArrayRef,
};
use arrow_schema::{DataType, Field as ArrowField};
use async_recursion::async_recursion;

use super::{Dictionary, LogicalType};
use crate::{
    arrow::*,
    encodings::Encoding,
    format::pb,
    io::object_reader::{read_binary_array, read_fixed_stride_array, ObjectReader},
    Error, Result,
};

/// Lance Schema Field
///
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub id: i32,
    parent_id: i32,
    logical_type: LogicalType,
    extension_name: String,
    pub(crate) encoding: Option<Encoding>,
    pub nullable: bool,

    pub children: Vec<Field>,

    /// Dictionary value array if this field is dictionary.
    pub dictionary: Option<Dictionary>,
}

impl Field {
    /// Returns arrow data type.
    pub fn data_type(&self) -> DataType {
        match &self.logical_type {
            lt if lt.is_list() => DataType::List(Arc::new(ArrowField::from(&self.children[0]))),
            lt if lt.is_large_list() => {
                DataType::LargeList(Arc::new(ArrowField::from(&self.children[0])))
            }
            lt if lt.is_struct() => {
                DataType::Struct(self.children.iter().map(ArrowField::from).collect())
            }
            lt => DataType::try_from(lt).unwrap(),
        }
    }

    pub fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|f| f.name == name)
    }

    pub fn child_mut(&mut self, name: &str) -> Option<&mut Self> {
        self.children.iter_mut().find(|f| f.name == name)
    }

    /// Attach the Dictionary's value array, so that we can later serialize
    /// the dictionary to the manifest.
    pub(crate) fn set_dictionary_values(&mut self, arr: &ArrayRef) {
        assert!(self.data_type().is_dictionary());
        // offset / length are set to 0 and recomputed when the dictionary is persisted to disk
        self.dictionary = Some(Dictionary {
            offset: 0,
            length: 0,
            values: Some(arr.clone()),
        });
    }

    pub(super) fn set_dictionary(&mut self, arr: &ArrayRef) {
        let data_type = self.data_type();
        match data_type {
            DataType::Dictionary(key_type, _) => match key_type.as_ref() {
                DataType::Int8 => {
                    self.set_dictionary_values(arr.as_dictionary::<Int8Type>().values())
                }
                DataType::Int16 => {
                    self.set_dictionary_values(arr.as_dictionary::<Int16Type>().values())
                }
                DataType::Int32 => {
                    self.set_dictionary_values(arr.as_dictionary::<Int32Type>().values())
                }
                DataType::Int64 => {
                    self.set_dictionary_values(arr.as_dictionary::<Int64Type>().values())
                }
                DataType::UInt8 => {
                    self.set_dictionary_values(arr.as_dictionary::<UInt8Type>().values())
                }
                DataType::UInt16 => {
                    self.set_dictionary_values(arr.as_dictionary::<UInt16Type>().values())
                }
                DataType::UInt32 => {
                    self.set_dictionary_values(arr.as_dictionary::<UInt32Type>().values())
                }
                DataType::UInt64 => {
                    self.set_dictionary_values(arr.as_dictionary::<UInt64Type>().values())
                }
                _ => {
                    panic!("Unsupported dictionary key type: {}", key_type);
                }
            },
            DataType::Struct(subfields) => {
                for (i, f) in subfields.iter().enumerate() {
                    let lance_field = self
                        .children
                        .iter_mut()
                        .find(|c| c.name == *f.name())
                        .unwrap();
                    let struct_arr = arr.as_struct();
                    lance_field.set_dictionary(struct_arr.column(i));
                }
            }
            DataType::List(_) => {
                let list_arr = arr.as_list::<i32>();
                self.children[0].set_dictionary(list_arr.values());
            }
            DataType::LargeList(_) => {
                let list_arr = arr.as_list::<i64>();
                self.children[0].set_dictionary(list_arr.values());
            }
            _ => {
                // Field types that don't support dictionaries
            }
        }
    }

    pub(super) fn sub_field(&self, path_components: &[&str]) -> Option<&Self> {
        if path_components.is_empty() {
            Some(self)
        } else {
            let first = path_components[0];
            self.children
                .iter()
                .find(|c| c.name == first)
                .and_then(|c| c.sub_field(&path_components[1..]))
        }
    }

    pub(crate) fn project(&self, path_components: &[&str]) -> Result<Self> {
        let mut f = Self {
            name: self.name.clone(),
            id: self.id,
            parent_id: self.parent_id,
            logical_type: self.logical_type.clone(),
            extension_name: self.extension_name.clone(),
            encoding: self.encoding.clone(),
            nullable: self.nullable,
            children: vec![],
            dictionary: self.dictionary.clone(),
        };
        if path_components.is_empty() {
            // Project stops here, copy all the remaining children.
            f.children = self.children.clone()
        } else {
            let first = path_components[0];
            for c in self.children.as_slice() {
                if c.name == first {
                    let projected = c.project(&path_components[1..])?;
                    f.children.push(projected);
                    break;
                }
            }
        }
        Ok(f)
    }

    /// Intersection of two [`Field`]s.
    ///
    pub(super) fn intersection(&self, other: &Self) -> Result<Self> {
        if self.name != other.name {
            return Err(Error::Arrow(format!(
                "Attempt to intersect different fields: {} and {}",
                self.name, other.name,
            )));
        }
        let self_type = self.data_type();
        let other_type = other.data_type();
        if self_type.is_struct() && other_type.is_struct() {
            let children = self
                .children
                .iter()
                .filter_map(|c| {
                    if let Some(other_child) = other.child(&c.name) {
                        let intersection = c.intersection(other_child).ok()?;
                        Some(intersection)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let f = Self {
                name: self.name.clone(),
                id: self.id,
                parent_id: self.parent_id,
                logical_type: self.logical_type.clone(),
                extension_name: self.extension_name.clone(),
                encoding: self.encoding.clone(),
                nullable: self.nullable,
                children,
                dictionary: self.dictionary.clone(),
            };
            return Ok(f);
        }

        if self_type != other_type || self.name != other.name {
            return Err(Error::Arrow(format!(
                "Attempt to intersect different fields: ({}, {}) and ({}, {})",
                self.name, self_type, other.name, other_type
            )));
        }

        Ok(self.clone())
    }

    pub(super) fn exclude(&self, other: &Self) -> Option<Self> {
        if !self.data_type().is_nested() {
            return None;
        }
        let children = self
            .children
            .iter()
            .map(|c| {
                if let Some(other_child) = other.child(&c.name) {
                    c.exclude(other_child)
                } else {
                    Some(c.clone())
                }
            })
            .filter(Option::is_some)
            .flatten()
            .collect::<Vec<_>>();
        if children.is_empty() {
            None
        } else {
            Some(Self {
                name: self.name.clone(),
                id: self.id,
                parent_id: self.parent_id,
                logical_type: self.logical_type.clone(),
                extension_name: self.extension_name.clone(),
                encoding: self.encoding.clone(),
                nullable: self.nullable,
                children,
                dictionary: self.dictionary.clone(),
            })
        }
    }

    /// Merge the children of other field into this one.
    pub(super) fn merge(&mut self, other: &Self) -> Result<()> {
        for other_child in other.children.as_slice() {
            if let Some(field) = self.child_mut(&other_child.name) {
                field.merge(other_child)?;
            } else {
                self.children.push(other_child.clone());
            }
        }
        Ok(())
    }

    // Get the max field id of itself and all children.
    pub(super) fn max_id(&self) -> i32 {
        max(
            self.id,
            self.children.iter().map(|c| c.max_id()).max().unwrap_or(-1),
        )
    }

    /// Recursively set field ID and parent ID for this field and all its children.
    pub(super) fn set_id(&mut self, parent_id: i32, id_seed: &mut i32) {
        self.parent_id = parent_id;
        if self.id < 0 {
            self.id = *id_seed;
            *id_seed += 1;
        }
        self.children
            .iter_mut()
            .for_each(|f| f.set_id(self.id, id_seed));
    }

    // Find any nested child with a specific field id
    pub(super) fn mut_field_by_id(&mut self, id: i32) -> Option<&mut Self> {
        for child in self.children.as_mut_slice() {
            if child.id == id {
                return Some(child);
            }
            if let Some(grandchild) = child.mut_field_by_id(id) {
                return Some(grandchild);
            }
        }
        None
    }

    #[async_recursion]
    pub(super) async fn load_dictionary<'a>(&mut self, reader: &dyn ObjectReader) -> Result<()> {
        if let DataType::Dictionary(_, value_type) = self.data_type() {
            assert!(self.dictionary.is_some());
            if let Some(dict_info) = self.dictionary.as_mut() {
                use DataType::*;
                match value_type.as_ref() {
                    Utf8 | Binary => {
                        dict_info.values = Some(
                            read_binary_array(
                                reader,
                                value_type.as_ref(),
                                false,
                                dict_info.offset,
                                dict_info.length,
                                ..,
                            )
                            .await?,
                        );
                    }
                    Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => {
                        dict_info.values = Some(
                            read_fixed_stride_array(
                                reader,
                                value_type.as_ref(),
                                dict_info.offset,
                                dict_info.length,
                                ..,
                            )
                            .await?,
                        );
                    }
                    _ => {
                        return Err(Error::Schema(format!(
                            "Does not support {} as dictionary value type",
                            value_type
                        )));
                    }
                }
            } else {
                panic!("Should not reach here: dictionary field does not load dictionary info")
            }
            Ok(())
        } else {
            for child in self.children.as_mut_slice() {
                child.load_dictionary(reader).await?;
            }
            Ok(())
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Field(id={}, name={}, type={})",
            self.id, self.name, self.logical_type.0,
        )
    }
}

impl TryFrom<&ArrowField> for Field {
    type Error = Error;

    fn try_from(field: &ArrowField) -> Result<Self> {
        let children = match field.data_type() {
            DataType::Struct(children) => children
                .iter()
                .map(|f| Self::try_from(f.as_ref()))
                .collect::<Result<_>>()?,
            DataType::List(item) => vec![Self::try_from(item.as_ref())?],
            DataType::LargeList(item) => vec![Self::try_from(item.as_ref())?],
            _ => vec![],
        };
        Ok(Self {
            id: -1,
            parent_id: -1,
            name: field.name().clone(),
            logical_type: LogicalType::try_from(field.data_type())?,
            encoding: match field.data_type() {
                dt if dt.is_fixed_stride() => Some(Encoding::Plain),
                dt if dt.is_binary_like() => Some(Encoding::VarBinary),
                DataType::Dictionary(_, _) => Some(Encoding::Dictionary),
                // Use plain encoder to store the offsets of list.
                DataType::List(_) | DataType::LargeList(_) => Some(Encoding::Plain),
                _ => None,
            },
            extension_name: "".to_string(),
            nullable: field.is_nullable(),
            children,
            dictionary: None,
        })
    }
}

impl TryFrom<ArrowField> for Field {
    type Error = Error;

    fn try_from(field: ArrowField) -> Result<Self> {
        Self::try_from(&field)
    }
}

impl From<&Field> for ArrowField {
    fn from(field: &Field) -> Self {
        Self::new(&field.name, field.data_type(), field.nullable)
    }
}

impl From<&pb::Field> for Field {
    fn from(field: &pb::Field) -> Self {
        Self {
            name: field.name.clone(),
            id: field.id,
            parent_id: field.parent_id,
            logical_type: LogicalType(field.logical_type.clone()),
            extension_name: field.extension_name.clone(),
            encoding: match field.encoding {
                1 => Some(Encoding::Plain),
                2 => Some(Encoding::VarBinary),
                3 => Some(Encoding::Dictionary),
                4 => Some(Encoding::RLE),
                _ => None,
            },
            nullable: field.nullable,
            children: vec![],
            dictionary: field.dictionary.as_ref().map(Dictionary::from),
        }
    }
}

impl From<&Field> for pb::Field {
    fn from(field: &Field) -> Self {
        Self {
            id: field.id,
            parent_id: field.parent_id,
            name: field.name.clone(),
            logical_type: field.logical_type.0.clone(),
            encoding: match field.encoding {
                Some(Encoding::Plain) => 1,
                Some(Encoding::VarBinary) => 2,
                Some(Encoding::Dictionary) => 3,
                Some(Encoding::RLE) => 4,
                _ => 0,
            },
            nullable: field.nullable,
            dictionary: field.dictionary.as_ref().map(pb::Dictionary::from),
            extension_name: field.extension_name.clone(),
            r#type: 0,
        }
    }
}

impl From<&Field> for Vec<pb::Field> {
    fn from(field: &Field) -> Self {
        let mut protos = vec![pb::Field::from(field)];
        protos.extend(field.children.iter().flat_map(Self::from));
        protos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_schema::{DataType, Fields, TimeUnit};

    #[test]
    fn arrow_field_to_field() {
        for (name, data_type) in [
            ("null", DataType::Null),
            ("bool", DataType::Boolean),
            ("int8", DataType::Int8),
            ("uint8", DataType::UInt8),
            ("int16", DataType::Int16),
            ("uint16", DataType::UInt16),
            ("int32", DataType::Int32),
            ("uint32", DataType::UInt32),
            ("int64", DataType::Int64),
            ("uint64", DataType::UInt64),
            ("float16", DataType::Float16),
            ("float32", DataType::Float32),
            ("float64", DataType::Float64),
            ("decimal128:7:3", DataType::Decimal128(7, 3)),
            ("timestamp:s:-", DataType::Timestamp(TimeUnit::Second, None)),
            (
                "timestamp:ms:-",
                DataType::Timestamp(TimeUnit::Millisecond, None),
            ),
            (
                "timestamp:us:-",
                DataType::Timestamp(TimeUnit::Microsecond, None),
            ),
            (
                "timestamp:ns:-",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
            ),
            (
                "timestamp:s:America/New_York",
                DataType::Timestamp(TimeUnit::Second, Some("America/New_York".into())),
            ),
            ("time32:s", DataType::Time32(TimeUnit::Second)),
            ("time32:ms", DataType::Time32(TimeUnit::Millisecond)),
            ("time64:us", DataType::Time64(TimeUnit::Microsecond)),
            ("time64:ns", DataType::Time64(TimeUnit::Nanosecond)),
            ("duration:s", DataType::Duration(TimeUnit::Second)),
            ("duration:ms", DataType::Duration(TimeUnit::Millisecond)),
            ("duration:us", DataType::Duration(TimeUnit::Microsecond)),
            ("duration:ns", DataType::Duration(TimeUnit::Nanosecond)),
            ("fixed_size_binary:100", DataType::FixedSizeBinary(100)),
            (
                "fixed_size_list:int32:10",
                DataType::FixedSizeList(
                    Arc::new(ArrowField::new("item", DataType::Int32, true)),
                    10,
                ),
            ),
        ] {
            let arrow_field = ArrowField::new(name, data_type.clone(), true);
            let field = Field::try_from(&arrow_field).unwrap();
            assert_eq!(field.name, name);
            assert_eq!(field.data_type(), data_type);
            assert_eq!(ArrowField::try_from(&field).unwrap(), arrow_field);
        }
    }

    #[test]
    fn test_nested_types() {
        assert_eq!(
            LogicalType::try_from(&DataType::List(Arc::new(ArrowField::new(
                "item",
                DataType::Binary,
                false
            ))))
            .unwrap()
            .0,
            "list"
        );
        assert_eq!(
            LogicalType::try_from(&DataType::List(Arc::new(ArrowField::new(
                "item",
                DataType::Struct(Fields::empty()),
                false
            ))))
            .unwrap()
            .0,
            "list.struct"
        );
        assert_eq!(
            LogicalType::try_from(&DataType::Struct(Fields::from(vec![ArrowField::new(
                "item",
                DataType::Binary,
                false
            )])))
            .unwrap()
            .0,
            "struct"
        );
    }

    #[test]
    fn struct_field() {
        let arrow_field = ArrowField::new(
            "struct",
            DataType::Struct(Fields::from(vec![ArrowField::new(
                "a",
                DataType::Int32,
                true,
            )])),
            false,
        );
        let field = Field::try_from(&arrow_field).unwrap();
        assert_eq!(field.name, "struct");
        assert_eq!(&field.data_type(), arrow_field.data_type());
        assert_eq!(ArrowField::try_from(&field).unwrap(), arrow_field);
    }

    #[test]
    fn test_field_intersection() {
        let f1: Field = ArrowField::new("a", DataType::Int32, true)
            .try_into()
            .unwrap();
        let f2: Field = ArrowField::new("a", DataType::Int32, true)
            .try_into()
            .unwrap();
        let i1 = f1.intersection(&f2).unwrap();

        assert_eq!(i1, f1);

        let f3: Field = ArrowField::new("b", DataType::Int32, true)
            .try_into()
            .unwrap();
        assert!(f1.intersection(&f3).is_err());
    }

    #[test]
    fn test_struct_field_intersection() {
        let f1: Field = ArrowField::new(
            "a",
            DataType::Struct(Fields::from(vec![
                ArrowField::new("b", DataType::Int32, true),
                ArrowField::new("c", DataType::Int32, true),
            ])),
            true,
        )
        .try_into()
        .unwrap();
        let f2: Field = ArrowField::new(
            "a",
            DataType::Struct(Fields::from(vec![
                ArrowField::new("c", DataType::Int32, true),
                ArrowField::new("a", DataType::Int32, true),
            ])),
            true,
        )
        .try_into()
        .unwrap();
        let actual = f1.intersection(&f2).unwrap();

        let expected: Field = ArrowField::new(
            "a",
            DataType::Struct(Fields::from(vec![ArrowField::new(
                "c",
                DataType::Int32,
                true,
            )])),
            true,
        )
        .try_into()
        .unwrap();
        assert_eq!(actual, expected);
    }
}
