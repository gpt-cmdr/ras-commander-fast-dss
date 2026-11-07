//! The file's variables with their group paths, flattened from `netcdf-reader`'s tree.
//! Source: NetCDF-Java 5.5.3 `Variable.getFullName`, `getUnitsString` and `getDescription`.

use super::refused;
use crate::error::VortexError;
use hdf5_reader::storage::Storage;
use netcdf_reader::{NcAttrValue, NcAttribute, NcGroup, NcType};

/// A dimension as `ucar.nc2.Dimension.equals` compares them: by name and length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dim {
    pub name: String,
    pub len: u64,
}

#[derive(Debug, Clone)]
pub(super) struct Var {
    /// `Grid/precipitation`.
    pub full: String,
    pub short: String,
    /// Index into [`Model::groups`].
    pub group: usize,
    pub dims: Vec<Dim>,
    pub dtype: NcType,
    pub attrs: Vec<NcAttribute>,
    /// `data_offset`: for HDF5, the address of the dataset's object header.
    pub address: u64,
}

#[derive(Debug)]
pub(super) struct Group {
    pub parent: Option<usize>,
    pub dims: Vec<Dim>,
}

#[derive(Debug)]
pub(super) struct Model {
    pub vars: Vec<Var>,
    pub groups: Vec<Group>,
    pub root_attrs: Vec<NcAttribute>,
}

impl Model {
    pub fn build(
        root: &NcGroup,
        max_variables: usize,
        storage: &dyn Storage,
    ) -> Result<Self, VortexError> {
        storage
            .charge(attributes_weight(&root.attributes))
            .map_err(|e| refused("NetCDF file", e.to_string()))?;
        let mut model = Model {
            vars: Vec::new(),
            groups: Vec::new(),
            root_attrs: root.attributes.clone(),
        };
        model.add(root, None, "", 0, max_variables, storage)?;
        model.name_empty_dimensions()?;
        Ok(model)
    }

    /// A dimension scale whose `NAME` attribute is empty comes back from `netcdf-reader` with an
    /// empty name. NetCDF-Java uses the scale's dataset name, so a one-dimensional variable on such
    /// a dimension names it. Scales of equal length could not be told apart here, so those are
    /// refused rather than guessed.
    fn name_empty_dimensions(&mut self) -> Result<(), VortexError> {
        let mut names: Vec<(usize, u64, String)> = Vec::new();
        for var in &self.vars {
            if let [d] = var.dims.as_slice() {
                if d.name.is_empty() {
                    names.push((var.group, d.len, var.short.clone()));
                }
            }
        }
        let lookup = |group: usize, len: u64| -> Result<String, VortexError> {
            let found: Vec<&(usize, u64, String)> = names
                .iter()
                .filter(|(g, l, _)| *g == group && *l == len)
                .collect();
            match found.as_slice() {
                [only] => Ok(only.2.clone()),
                [] => Err(refused(
                    "NetCDF dimension",
                    format!("an unnamed dimension of length {len} has no scale"),
                )),
                _ => Err(refused(
                    "NetCDF dimension",
                    format!("several unnamed scales have length {len}"),
                )),
            }
        };
        for var in &mut self.vars {
            for d in &mut var.dims {
                if d.name.is_empty() {
                    d.name = lookup(var.group, d.len)?;
                }
            }
        }
        for (index, group) in self.groups.iter_mut().enumerate() {
            for d in &mut group.dims {
                if d.name.is_empty() {
                    d.name = lookup(index, d.len)?;
                }
            }
        }
        Ok(())
    }

    fn add(
        &mut self,
        group: &NcGroup,
        parent: Option<usize>,
        prefix: &str,
        depth: usize,
        max_variables: usize,
        storage: &dyn Storage,
    ) -> Result<(), VortexError> {
        if depth > 32 {
            return Err(refused("NetCDF groups", "nested over 32 levels"));
        }
        let index = self.groups.len();
        let dims = |list: &[netcdf_reader::NcDimension]| -> Vec<Dim> {
            list.iter()
                .map(|d| Dim {
                    name: d.name.clone(),
                    len: d.size,
                })
                .collect()
        };
        self.groups.push(Group {
            parent,
            dims: dims(&group.dimensions),
        });
        for v in &group.variables {
            if self.vars.len() >= max_variables {
                return Err(refused(
                    "NetCDF file",
                    format!("over {max_variables} variables"),
                ));
            }
            // A copy of what the reader decoded, which a file can make large.
            storage
                .charge(attributes_weight(&v.attributes))
                .map_err(|e| refused("NetCDF file", e.to_string()))?;
            self.vars.push(Var {
                full: format!("{prefix}{}", v.name),
                short: v.name.clone(),
                group: index,
                dims: dims(&v.dimensions),
                dtype: v.dtype.clone(),
                attrs: v.attributes.clone(),
                address: v.data_offset,
            });
        }
        for g in &group.groups {
            let path = format!("{prefix}{}/", g.name);
            self.add(g, Some(index), &path, depth + 1, max_variables, storage)?;
        }
        Ok(())
    }

    /// `CoordSystemBuilder.findVarProcess`: full name, then the same group, then any short name.
    pub fn find(&self, name: &str, from: usize) -> Option<usize> {
        let group = self.vars[from].group;
        self.vars
            .iter()
            .position(|v| v.full == name)
            .or_else(|| {
                self.vars
                    .iter()
                    .position(|v| v.short == name && v.group == group)
            })
            .or_else(|| self.vars.iter().position(|v| v.short == name))
    }

    /// `Group.findVariableLocal`.
    pub fn find_local(&self, name: &str, group: usize) -> Option<usize> {
        self.vars
            .iter()
            .position(|v| v.short == name && v.group == group)
    }

    /// The group declaring a dimension, searching outward from `group`.
    pub fn dim_owner(&self, name: &str, group: usize) -> Option<usize> {
        let mut at = Some(group);
        while let Some(g) = at {
            if self.groups[g].dims.iter().any(|d| d.name == name) {
                return Some(g);
            }
            at = self.groups[g].parent;
        }
        None
    }
}

fn attributes_weight(attrs: &[NcAttribute]) -> u64 {
    attrs.iter().fold(0u64, |all, a| {
        let values = match &a.value {
            NcAttrValue::Chars(s) => s.len(),
            NcAttrValue::Strings(v) => v.iter().map(|s| s.len() + size_of::<String>()).sum(),
            NcAttrValue::Bytes(v) => v.len(),
            NcAttrValue::UBytes(v) => v.len(),
            NcAttrValue::Shorts(v) => v.len() * 2,
            NcAttrValue::UShorts(v) => v.len() * 2,
            NcAttrValue::Ints(v) => v.len() * 4,
            NcAttrValue::UInts(v) => v.len() * 4,
            NcAttrValue::Floats(v) => v.len() * 4,
            NcAttrValue::Doubles(v) => v.len() * 8,
            NcAttrValue::Int64s(v) => v.len() * 8,
            NcAttrValue::UInt64s(v) => v.len() * 8,
            // Weighed in the walk, field by field.
            NcAttrValue::UserDefined { values, .. } => {
                values.len() * size_of::<netcdf_reader::NcValue>()
            }
        };
        all.saturating_add((values + a.name.len() + size_of::<NcAttribute>()) as u64)
    })
}

pub(super) fn find_attr<'a>(attrs: &'a [NcAttribute], name: &str) -> Option<&'a NcAttribute> {
    attrs.iter().find(|a| a.name == name)
}

pub(super) fn find_attr_ignore_case<'a>(
    attrs: &'a [NcAttribute],
    name: &str,
) -> Option<&'a NcAttribute> {
    attrs.iter().find(|a| a.name.eq_ignore_ascii_case(name))
}

/// `Attribute.isString` values: a character array or a single string. NetCDF-Java takes the first
/// of several strings.
pub(super) fn attr_string(attr: &NcAttribute) -> Option<String> {
    match &attr.value {
        NcAttrValue::Chars(s) => Some(s.trim_end_matches('\0').to_string()),
        NcAttrValue::Strings(s) => s.first().cloned(),
        _ => None,
    }
}

pub(super) fn string_attr(attrs: &[NcAttribute], name: &str) -> Option<String> {
    find_attr(attrs, name).and_then(attr_string)
}

/// `Attribute.getNumericValue(i).doubleValue()`, keeping the attribute's signed reading.
pub(super) fn attr_numbers(attr: &NcAttribute) -> Option<Vec<f64>> {
    Some(match &attr.value {
        NcAttrValue::Bytes(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::Shorts(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::Ints(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::Floats(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::Doubles(v) => v.clone(),
        NcAttrValue::UBytes(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::UShorts(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::UInts(v) => v.iter().map(|x| f64::from(*x)).collect(),
        NcAttrValue::Int64s(v) => v.iter().map(|x| *x as f64).collect(),
        NcAttrValue::UInt64s(v) => v.iter().map(|x| *x as f64).collect(),
        _ => return None,
    })
}

pub(super) fn number_attr(attrs: &[NcAttribute], name: &str) -> Option<f64> {
    find_attr(attrs, name)
        .and_then(attr_numbers)
        .and_then(|v| v.first().copied())
}

/// `Variable.getUnitsString`.
pub(super) fn units(var: &Var) -> Option<String> {
    find_attr_ignore_case(&var.attrs, "units")
        .and_then(attr_string)
        .map(|s| s.trim().to_string())
}

/// `Variable.getDescription`.
pub(super) fn description(var: &Var) -> Option<String> {
    ["long_name", "description", "title", "standard_name"]
        .iter()
        .find_map(|name| find_attr_ignore_case(&var.attrs, name).and_then(attr_string))
}
