//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.2

use async_graphql::SimpleObject;
use boilermates::boilermates;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    migrator::{MetadataLot, MetadataSource},
    models::media::MetadataImages,
};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize, SimpleObject)]
#[sea_orm(table_name = "metadata_group")]
#[graphql(name = "MetadataGroup")]
#[boilermates("MetadataGroupWithoutId")]
pub struct Model {
    #[boilermates(not_in("MetadataGroupWithoutId"))]
    #[sea_orm(primary_key)]
    pub id: i32,
    pub parts: i32,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    #[graphql(skip)]
    pub images: MetadataImages,
    #[sea_orm(ignore)]
    pub display_images: Vec<String>,
    pub lot: MetadataLot,
    pub source: MetadataSource,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::partial_metadata_to_metadata_group::Entity")]
    PartialMetadataToMetadataGroup,
}

impl Related<super::partial_metadata_to_metadata_group::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PartialMetadataToMetadataGroup.def()
    }
}

impl Related<super::partial_metadata::Entity> for Entity {
    fn to() -> RelationDef {
        super::partial_metadata_to_metadata_group::Relation::PartialMetadata.def()
    }
    fn via() -> Option<RelationDef> {
        Some(
            super::partial_metadata_to_metadata_group::Relation::MetadataGroup
                .def()
                .rev(),
        )
    }
}

impl ActiveModelBehavior for ActiveModel {}
