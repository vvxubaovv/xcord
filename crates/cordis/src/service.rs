use std::any::{Any, TypeId};
use std::rc::Rc;

use crate::PluginId;

pub(crate) struct ServiceSlot {
    pub(crate) type_id: TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) value: Rc<dyn Any>,
    pub(crate) owner: Option<PluginId>,
}

impl ServiceSlot {
    pub(crate) fn new<T: 'static>(value: Rc<T>, owner: Option<PluginId>) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            value,
            owner,
        }
    }
}
