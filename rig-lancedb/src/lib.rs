use lancedb::{
    index::Index,
    query::{QueryBase, VectorQuery},
    DistanceType,
};
use rig::{
    embeddings::EmbeddingModel,
    vector_store::{VectorStoreError, VectorStoreIndex},
};
use serde::Deserialize;
use serde_json::Value;
use utils::Query;

mod utils;

fn lancedb_to_rig_error(e: lancedb::Error) -> VectorStoreError {
    VectorStoreError::DatastoreError(Box::new(e))
}

fn serde_to_rig_error(e: serde_json::Error) -> VectorStoreError {
    VectorStoreError::JsonError(e)
}

pub struct LanceDbVectorStore<M: EmbeddingModel> {
    /// Defines which model is used to generate embeddings for the vector store
    model: M,
    table: lancedb::Table,
    id_field: String,
    /// Vector search params that are used during vector search operations.
    search_params: SearchParams,
}

impl<M: EmbeddingModel> LanceDbVectorStore<M> {
    fn build_query(&self, mut query: VectorQuery) -> VectorQuery {
        let SearchParams {
            distance_type,
            search_type,
            nprobes,
            refine_factor,
            post_filter,
        } = self.search_params.clone();

        if let Some(distance_type) = distance_type {
            query = query.distance_type(distance_type);
        }

        if let Some(SearchType::Flat) = search_type {
            query = query.bypass_vector_index();
        }

        if let Some(SearchType::Approximate) = search_type {
            if let Some(nprobes) = nprobes {
                query = query.nprobes(nprobes);
            }
            if let Some(refine_factor) = refine_factor {
                query = query.refine_factor(refine_factor);
            }
        }

        if let Some(true) = post_filter {
            query = query.postfilter();
        }

        query
    }
}

/// See [LanceDB vector search](https://lancedb.github.io/lancedb/search/) for more information.
#[derive(Debug, Clone)]
pub enum SearchType {
    // Flat search, also called ENN or kNN.
    Flat,
    /// Approximal Nearest Neighbor search, also called ANN.
    Approximate,
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    /// Always set the distance_type to match the value used to train the index
    /// By default, set to L2
    distance_type: Option<DistanceType>,
    /// By default, ANN will be used if there is an index on the table.
    /// By default, kNN will be used if there is NO index on the table.
    /// To use defaults, set to None.
    search_type: Option<SearchType>,
    /// Set this value only when search type is ANN.
    /// See [LanceDb ANN Search](https://lancedb.github.io/lancedb/ann_indexes/#querying-an-ann-index) for more information
    nprobes: Option<usize>,
    /// Set this value only when search type is ANN.
    /// See [LanceDb ANN Search](https://lancedb.github.io/lancedb/ann_indexes/#querying-an-ann-index) for more information
    refine_factor: Option<u32>,
    /// If set to true, filtering will happen after the vector search instead of before
    /// See [LanceDb pre/post filtering](https://lancedb.github.io/lancedb/sql/#pre-and-post-filtering) for more information
    post_filter: Option<bool>,
}

impl SearchParams {
    pub fn distance_type(mut self, distance_type: DistanceType) -> Self {
        self.distance_type = Some(distance_type);
        self
    }

    pub fn search_type(mut self, search_type: SearchType) -> Self {
        self.search_type = Some(search_type);
        self
    }

    pub fn nprobes(mut self, nprobes: usize) -> Self {
        self.nprobes = Some(nprobes);
        self
    }

    pub fn refine_factor(mut self, refine_factor: u32) -> Self {
        self.refine_factor = Some(refine_factor);
        self
    }

    pub fn post_filter(mut self, post_filter: bool) -> Self {
        self.post_filter = Some(post_filter);
        self
    }
}

impl<M: EmbeddingModel> LanceDbVectorStore<M> {
    pub async fn new(
        table: lancedb::Table,
        model: M,
        id_field: &str,
        search_params: SearchParams,
    ) -> Result<Self, lancedb::Error> {
        Ok(Self {
            table,
            model,
            id_field: id_field.to_string(),
            search_params,
        })
    }

    /// Define index on document table `id` field for search optimization.
    pub async fn create_index(
        &self,
        index: Index,
        field_names: &[impl AsRef<str>],
    ) -> Result<(), lancedb::Error> {
        self.table.create_index(field_names, index).execute().await
    }
}

impl<M: EmbeddingModel + std::marker::Sync + Send> VectorStoreIndex for LanceDbVectorStore<M> {
    async fn top_n<T: for<'a> Deserialize<'a> + std::marker::Send>(
        &self,
        query: &str,
        n: usize,
    ) -> Result<Vec<(f64, String, T)>, VectorStoreError> {
        let prompt_embedding = self.model.embed_document(query).await?;

        let query = self
            .table
            .vector_search(prompt_embedding.vec.clone())
            .map_err(lancedb_to_rig_error)?
            .limit(n);

        self.build_query(query)
            .execute_query()
            .await?
            .into_iter()
            .map(|value| {
                Ok((
                    match value.get("_distance") {
                        Some(Value::Number(distance)) => distance.as_f64().unwrap_or_default(),
                        _ => 0.0,
                    },
                    match value.get(self.id_field.clone()) {
                        Some(Value::String(id)) => id.to_string(),
                        _ => "".to_string(),
                    },
                    serde_json::from_value(value).map_err(serde_to_rig_error)?,
                ))
            })
            .collect()
    }

    async fn top_n_ids(
        &self,
        query: &str,
        n: usize,
    ) -> Result<Vec<(f64, String)>, VectorStoreError> {
        let prompt_embedding = self.model.embed_document(query).await?;

        let query = self
            .table
            .query()
            .select(lancedb::query::Select::Columns(vec![self.id_field.clone()]))
            .nearest_to(prompt_embedding.vec.clone())
            .map_err(lancedb_to_rig_error)?
            .limit(n);

        self.build_query(query)
            .execute_query()
            .await?
            .into_iter()
            .map(|value| {
                Ok((
                    match value.get("distance") {
                        Some(Value::Number(distance)) => distance.as_f64().unwrap_or_default(),
                        _ => 0.0,
                    },
                    match value.get(self.id_field.clone()) {
                        Some(Value::String(id)) => id.to_string(),
                        _ => "".to_string(),
                    },
                ))
            })
            .collect()
    }
}
