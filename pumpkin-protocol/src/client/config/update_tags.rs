use std::io::Write;

use pumpkin_data::{
    fluid::Fluid,
    packet::clientbound::CONFIG_UPDATE_TAGS,
    tag::{RegistryKey, get_registry_key_tags},
};
use pumpkin_macros::packet;
use pumpkin_world::block::registry;

use crate::{
    ClientPacket,
    codec::{identifier::Identifier, var_int::VarInt},
    ser::{NetworkWriteExt, WritingError},
};

#[packet(CONFIG_UPDATE_TAGS)]
pub struct CUpdateTags<'a> {
    tags: &'a [pumpkin_data::tag::RegistryKey],
}

impl<'a> CUpdateTags<'a> {
    pub fn new(tags: &'a [pumpkin_data::tag::RegistryKey]) -> Self {
        Self { tags }
    }
}

impl ClientPacket for CUpdateTags<'_> {
    fn write_packet_data(&self, write: impl Write) -> Result<(), WritingError> {
        let mut write = write;
        write.write_list(self.tags, |p, registry_key| {
            p.write_identifier(&Identifier::vanilla(registry_key.identifier_string()))?;

            let values = get_registry_key_tags(registry_key);
            p.write_var_int(&VarInt::from(values.len()))?;
            for (key, values) in values.iter() {
                // This is technically an `Identifier` but same thing
                p.write_string_bounded(key, u16::MAX as usize)?;
                p.write_list(values, |p, string_id| {
                    let id = match registry_key {
                        RegistryKey::Block => registry::get_block(string_id).unwrap().id as i32,
                        RegistryKey::Fluid => Fluid::ident_to_fluid_id(string_id).unwrap() as i32,
                        _ => unimplemented!(),
                    };

                    p.write_var_int(&VarInt::from(id))
                })?;
            }

            Ok(())
        })
    }
}
