use super::ids::*;
use super::DatabaseError;
use std::collections::HashMap;

pub struct VersionBuilder {
    pub version_id: VersionId,
    pub project_id: ProjectId,
    pub author_id: UserId,
    pub name: String,
    pub version_number: String,
    pub changelog: String,
    pub files: Vec<VersionFileBuilder>,
    pub dependencies: Vec<DependencyBuilder>,
    pub game_versions: Vec<GameVersionId>,
    pub loaders: Vec<LoaderId>,
    pub release_channel: ChannelId,
    pub featured: bool,
}

pub struct DependencyBuilder {
    pub project_id: Option<ProjectId>,
    pub version_id: Option<VersionId>,
    pub dependency_type: String,
}

impl DependencyBuilder {
    pub async fn insert(
        self,
        version_id: VersionId,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), DatabaseError> {
        let version_dependency_id = if let Some(project_id) = self.project_id {
            sqlx::query!(
                "
                SELECT version.id id FROM (
                    SELECT DISTINCT ON(v.id) v.id, v.date_published FROM versions v
                    INNER JOIN game_versions_versions gvv ON gvv.joining_version_id = v.id AND gvv.game_version_id IN (SELECT game_version_id FROM game_versions_versions WHERE joining_version_id = $2)
                    INNER JOIN loaders_versions lv ON lv.version_id = v.id AND lv.loader_id IN (SELECT loader_id FROM loaders_versions WHERE version_id = $2)
                    WHERE v.mod_id = $1
                ) AS version
                ORDER BY version.date_published DESC
                LIMIT 1
                ",
                project_id as ProjectId,
                version_id as VersionId,
            )
                .fetch_optional(&mut *transaction).await?.map(|x| VersionId(x.id))
        } else {
            self.version_id
        };

        sqlx::query!(
            "
            INSERT INTO dependencies (dependent_id, dependency_type, dependency_id, mod_dependency_id)
            VALUES ($1, $2, $3, $4)
            ",
            version_id as VersionId,
            self.dependency_type,
            version_dependency_id.map(|x| x.0),
            self.project_id.map(|x| x.0),
        )
        .execute(&mut *transaction)
        .await?;

        Ok(())
    }
}

pub struct VersionFileBuilder {
    pub url: String,
    pub filename: String,
    pub hashes: Vec<HashBuilder>,
    pub primary: bool,
}

impl VersionFileBuilder {
    pub async fn insert(
        self,
        version_id: VersionId,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<FileId, DatabaseError> {
        let file_id = generate_file_id(&mut *transaction).await?;

        sqlx::query!(
            "
            INSERT INTO files (id, version_id, url, filename)
            VALUES ($1, $2, $3, $4)
            ",
            file_id as FileId,
            version_id as VersionId,
            self.url,
            self.filename,
        )
        .execute(&mut *transaction)
        .await?;

        for hash in self.hashes {
            sqlx::query!(
                "
                INSERT INTO hashes (file_id, algorithm, hash)
                VALUES ($1, $2, $3)
                ",
                file_id as FileId,
                hash.algorithm,
                hash.hash,
            )
            .execute(&mut *transaction)
            .await?;
        }

        Ok(file_id)
    }
}

pub struct HashBuilder {
    pub algorithm: String,
    pub hash: Vec<u8>,
}

impl VersionBuilder {
    pub async fn insert(
        self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<VersionId, DatabaseError> {
        let version = Version {
            id: self.version_id,
            project_id: self.project_id,
            author_id: self.author_id,
            name: self.name,
            version_number: self.version_number,
            changelog: self.changelog,
            changelog_url: None,
            date_published: chrono::Utc::now(),
            downloads: 0,
            release_channel: self.release_channel,
            featured: self.featured,
        };

        version.insert(&mut *transaction).await?;

        sqlx::query!(
            "
            UPDATE mods
            SET updated = NOW()
            WHERE id = $1
            ",
            self.project_id as ProjectId,
        )
        .execute(&mut *transaction)
        .await?;

        for file in self.files {
            file.insert(self.version_id, transaction).await?;
        }

        for dependency in self.dependencies {
            dependency.insert(self.version_id, transaction).await?;
        }

        for loader in self.loaders.clone() {
            sqlx::query!(
                "
                INSERT INTO loaders_versions (loader_id, version_id)
                VALUES ($1, $2)
                ",
                loader as LoaderId,
                self.version_id as VersionId,
            )
            .execute(&mut *transaction)
            .await?;
        }

        for game_version in self.game_versions.clone() {
            sqlx::query!(
                "
                INSERT INTO game_versions_versions (game_version_id, joining_version_id)
                VALUES ($1, $2)
                ",
                game_version as GameVersionId,
                self.version_id as VersionId,
            )
            .execute(&mut *transaction)
            .await?;
        }

        // Sync dependencies

        use futures::stream::TryStreamExt;

        let dependencies = sqlx::query!(
            "
            SELECT d.id id
            FROM versions v
            INNER JOIN dependencies d ON d.dependent_id = v.id
            INNER JOIN game_versions_versions gvv ON gvv.joining_version_id = v.id AND gvv.game_version_id IN (SELECT * FROM UNNEST($2::integer[]))
            INNER JOIN loaders_versions lv ON lv.version_id = v.id AND lv.loader_id IN (SELECT * FROM UNNEST($3::integer[]))
            WHERE v.mod_id = $1
            ",
            self.project_id as ProjectId,
            &self.game_versions.iter().map(|x| x.0).collect::<Vec<i32>>(),
            &self.loaders.iter().map(|x| x.0).collect::<Vec<i32>>(),
        )
            .fetch_many(&mut *transaction)
            .try_filter_map(|e| async {
                Ok(e.right().map(|d| d.id as i64))
            })
            .try_collect::<Vec<i64>>()
            .await?;

        sqlx::query!(
            "
            UPDATE dependencies
            SET dependency_id = $2
            WHERE id IN (SELECT * FROM UNNEST($1::bigint[]))
            ",
            &dependencies,
            self.version_id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        Ok(self.version_id)
    }
}

pub struct Version {
    pub id: VersionId,
    pub project_id: ProjectId,
    pub author_id: UserId,
    pub name: String,
    pub version_number: String,
    pub changelog: String,
    pub changelog_url: Option<String>,
    pub date_published: chrono::DateTime<chrono::Utc>,
    pub downloads: i32,
    pub release_channel: ChannelId,
    pub featured: bool,
}

impl Version {
    pub async fn insert(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), sqlx::error::Error> {
        sqlx::query!(
            "
            INSERT INTO versions (
                id, mod_id, author_id, name, version_number,
                changelog, changelog_url, date_published,
                downloads, release_channel, featured
            )
            VALUES (
                $1, $2, $3, $4, $5,
                $6, $7,
                $8, $9,
                $10, $11
            )
            ",
            self.id as VersionId,
            self.project_id as ProjectId,
            self.author_id as UserId,
            &self.name,
            &self.version_number,
            self.changelog,
            self.changelog_url.as_ref(),
            self.date_published,
            self.downloads,
            self.release_channel as ChannelId,
            self.featured
        )
        .execute(&mut *transaction)
        .await?;

        Ok(())
    }

    // TODO: someone verify this
    pub async fn remove_full(
        id: VersionId,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<Option<()>, sqlx::Error> {
        let result = sqlx::query!(
            "
            SELECT EXISTS(SELECT 1 FROM versions WHERE id = $1)
            ",
            id as VersionId,
        )
        .fetch_one(&mut *transaction)
        .await?;

        if !result.exists.unwrap_or(false) {
            return Ok(None);
        }

        sqlx::query!(
            "
            DELETE FROM reports
            WHERE version_id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        use futures::TryStreamExt;

        let game_versions: Vec<i32> = sqlx::query!(
            "
                SELECT game_version_id id FROM game_versions_versions
                WHERE joining_version_id = $1
                ",
            id as VersionId,
        )
        .fetch_many(&mut *transaction)
        .try_filter_map(|e| async { Ok(e.right().map(|c| c.id)) })
        .try_collect::<Vec<i32>>()
        .await?;

        let loaders: Vec<i32> = sqlx::query!(
            "
                SELECT loader_id id FROM loaders_versions
                WHERE version_id = $1
                ",
            id as VersionId,
        )
        .fetch_many(&mut *transaction)
        .try_filter_map(|e| async { Ok(e.right().map(|c| c.id)) })
        .try_collect::<Vec<i32>>()
        .await?;

        sqlx::query!(
            "
            DELETE FROM game_versions_versions gvv
            WHERE gvv.joining_version_id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        sqlx::query!(
            "
            DELETE FROM loaders_versions
            WHERE loaders_versions.version_id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        sqlx::query!(
            "
            DELETE FROM downloads
            WHERE downloads.version_id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        let files = sqlx::query!(
            "
            SELECT files.id, files.url, files.filename, files.is_primary FROM files
            WHERE files.version_id = $1
            ",
            id as VersionId,
        )
        .fetch_many(&mut *transaction)
        .try_filter_map(|e| async {
            Ok(e.right().map(|c| VersionFile {
                id: FileId(c.id),
                version_id: id,
                url: c.url,
                filename: c.filename,
                primary: c.is_primary,
            }))
        })
        .try_collect::<Vec<VersionFile>>()
        .await?;

        for file in files {
            // TODO: store backblaze id in database so that we can delete the files here
            // For now, we can't delete the files since we don't have the backblaze id
            log::warn!(
                "Can't delete version file id: {} (url: {}, name: {})",
                file.id.0,
                file.url,
                file.filename
            )
        }

        sqlx::query!(
            "
            DELETE FROM hashes
            WHERE EXISTS(
                SELECT 1 FROM files WHERE
                    (files.version_id = $1) AND
                    (hashes.file_id = files.id)
            )
            ",
            id as VersionId
        )
        .execute(&mut *transaction)
        .await?;

        sqlx::query!(
            "
            DELETE FROM files
            WHERE files.version_id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;

        // Sync dependencies

        let project_id = sqlx::query!(
            "
            SELECT mod_id FROM versions WHERE id = $1
            ",
            id as VersionId,
        )
        .fetch_one(&mut *transaction)
        .await?;

        let new_version_id = sqlx::query!(
            "
            SELECT v.id id
            FROM versions v
            INNER JOIN game_versions_versions gvv ON gvv.joining_version_id = v.id AND gvv.game_version_id IN (SELECT * FROM UNNEST($2::integer[]))
            INNER JOIN loaders_versions lv ON lv.version_id = v.id AND lv.loader_id IN (SELECT * FROM UNNEST($3::integer[]))
            WHERE v.mod_id = $1
            ORDER BY v.date_published DESC
            LIMIT 1
            ",
            project_id.mod_id,
            &game_versions,
            &loaders,
        )
            .fetch_optional(&mut *transaction)
            .await?
            .map(|x| x.id);

        sqlx::query!(
            "
            UPDATE dependencies
            SET dependency_id = $2
            WHERE dependency_id = $1
            ",
            id as VersionId,
            new_version_id,
        )
        .execute(&mut *transaction)
        .await?;

        sqlx::query!(
            "
            DELETE FROM dependencies WHERE mod_dependency_id = NULL AND dependency_id = NULL
            ",
        )
        .execute(&mut *transaction)
        .await?;

        // delete version

        sqlx::query!(
            "
            DELETE FROM versions WHERE id = $1
            ",
            id as VersionId,
        )
        .execute(&mut *transaction)
        .await?;
        Ok(Some(()))
    }

    pub async fn get_project_versions<'a, E>(
        project_id: ProjectId,
        game_versions: Option<Vec<String>>,
        loaders: Option<Vec<String>>,
        exec: E,
    ) -> Result<Vec<VersionId>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        use futures::stream::TryStreamExt;

        let vec = sqlx::query!(
            "
            SELECT version.id FROM (
                SELECT DISTINCT ON(v.id) v.id, v.date_published FROM versions v
                INNER JOIN game_versions_versions gvv ON gvv.joining_version_id = v.id
                INNER JOIN game_versions gv on gvv.game_version_id = gv.id AND (cardinality($2::varchar[]) = 0 OR gv.version = ANY($2::varchar[]))
                INNER JOIN loaders_versions lv ON lv.version_id = v.id
                INNER JOIN loaders l on lv.loader_id = l.id AND (cardinality($3::varchar[]) = 0 OR l.loader = ANY($3::varchar[]))
                WHERE v.mod_id = $1
            ) AS version
            ORDER BY version.date_published ASC
            ",
            project_id as ProjectId,
            &game_versions.unwrap_or_default(),
            &loaders.unwrap_or_default(),
        )
        .fetch_many(exec)
        .try_filter_map(|e| async { Ok(e.right().map(|v| VersionId(v.id))) })
        .try_collect::<Vec<VersionId>>()
        .await?;

        Ok(vec)
    }

    pub async fn get<'a, 'b, E>(
        id: VersionId,
        executor: E,
    ) -> Result<Option<Self>, sqlx::error::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        let result = sqlx::query!(
            "
            SELECT v.mod_id, v.author_id, v.name, v.version_number,
                v.changelog, v.changelog_url, v.date_published, v.downloads,
                v.release_channel, v.featured
            FROM versions v
            WHERE v.id = $1
            ",
            id as VersionId,
        )
        .fetch_optional(executor)
        .await?;

        if let Some(row) = result {
            Ok(Some(Version {
                id,
                project_id: ProjectId(row.mod_id),
                author_id: UserId(row.author_id),
                name: row.name,
                version_number: row.version_number,
                changelog: row.changelog,
                changelog_url: row.changelog_url,
                date_published: row.date_published,
                downloads: row.downloads,
                release_channel: ChannelId(row.release_channel),
                featured: row.featured,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn get_many<'a, E>(
        version_ids: Vec<VersionId>,
        exec: E,
    ) -> Result<Vec<Version>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Copy,
    {
        use futures::stream::TryStreamExt;

        let version_ids_parsed: Vec<i64> = version_ids.into_iter().map(|x| x.0).collect();
        let versions = sqlx::query!(
            "
            SELECT v.id, v.mod_id, v.author_id, v.name, v.version_number,
                v.changelog, v.changelog_url, v.date_published, v.downloads,
                v.release_channel, v.featured
            FROM versions v
            WHERE v.id IN (SELECT * FROM UNNEST($1::bigint[]))
            ORDER BY v.date_published ASC
            ",
            &version_ids_parsed
        )
        .fetch_many(exec)
        .try_filter_map(|e| async {
            Ok(e.right().map(|v| Version {
                id: VersionId(v.id),
                project_id: ProjectId(v.mod_id),
                author_id: UserId(v.author_id),
                name: v.name,
                version_number: v.version_number,
                changelog: v.changelog,
                changelog_url: v.changelog_url,
                date_published: v.date_published,
                downloads: v.downloads,
                release_channel: ChannelId(v.release_channel),
                featured: v.featured,
            }))
        })
        .try_collect::<Vec<Version>>()
        .await?;

        Ok(versions)
    }

    pub async fn get_full<'a, 'b, E>(
        id: VersionId,
        executor: E,
    ) -> Result<Option<QueryVersion>, sqlx::error::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
    {
        let result = sqlx::query!(
            "
            SELECT v.id id, v.mod_id mod_id, v.author_id author_id, v.name version_name, v.version_number version_number,
            v.changelog changelog, v.changelog_url changelog_url, v.date_published date_published, v.downloads downloads,
            rc.channel release_channel, v.featured featured,
            STRING_AGG(DISTINCT gv.version, ',') game_versions, STRING_AGG(DISTINCT l.loader, ',') loaders,
            STRING_AGG(DISTINCT f.id || ', ' || f.filename || ', ' || f.is_primary || ', ' || f.url, ' ,') files,
            STRING_AGG(DISTINCT h.algorithm || ', ' || encode(h.hash, 'escape') || ', ' || h.file_id,  ' ,') hashes,
            STRING_AGG(DISTINCT COALESCE(d.dependency_id, 0) || ', ' || COALESCE(d.mod_dependency_id, 0) || ', ' || d.dependency_type,  ' ,') dependencies
            FROM versions v
            INNER JOIN release_channels rc on v.release_channel = rc.id
            LEFT OUTER JOIN game_versions_versions gvv on v.id = gvv.joining_version_id
            LEFT OUTER JOIN game_versions gv on gvv.game_version_id = gv.id
            LEFT OUTER JOIN loaders_versions lv on v.id = lv.version_id
            LEFT OUTER JOIN loaders l on lv.loader_id = l.id
            LEFT OUTER JOIN files f on v.id = f.version_id
            LEFT OUTER JOIN hashes h on f.id = h.file_id
            LEFT OUTER JOIN dependencies d on v.id = d.dependent_id
            WHERE v.id = $1
            GROUP BY rc.id, v.id;
            ",
            id as VersionId,
        )
            .fetch_optional(executor)
            .await?;

        if let Some(v) = result {
            let hashes: Vec<(FileId, String, Vec<u8>)> = v
                .hashes
                .unwrap_or_default()
                .split(" ,")
                .map(|f| {
                    let hash: Vec<&str> = f.split(", ").collect();

                    if hash.len() >= 3 {
                        Some((
                            FileId(hash[2].parse().unwrap_or(0)),
                            hash[0].to_string(),
                            hash[1].to_string().into_bytes(),
                        ))
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();

            Ok(Some(QueryVersion {
                id: VersionId(v.id),
                project_id: ProjectId(v.mod_id),
                author_id: UserId(v.author_id),
                name: v.version_name,
                version_number: v.version_number,
                changelog: v.changelog,
                changelog_url: v.changelog_url,
                date_published: v.date_published,
                downloads: v.downloads,
                release_channel: v.release_channel,
                files: v
                    .files
                    .unwrap_or_default()
                    .split(" ,")
                    .map(|f| {
                        let file: Vec<&str> = f.split(", ").collect();

                        if file.len() >= 4 {
                            let file_id = FileId(file[0].parse().unwrap_or(0));
                            let mut file_hashes = HashMap::new();

                            for hash in &hashes {
                                if (hash.0).0 == file_id.0 {
                                    file_hashes.insert(hash.1.clone(), hash.2.clone());
                                }
                            }

                            Some(QueryFile {
                                id: file_id,
                                url: file[3].to_string(),
                                filename: file[1].to_string(),
                                hashes: file_hashes,
                                primary: file[2].parse().unwrap_or(false),
                            })
                        } else {
                            None
                        }
                    })
                    .flatten()
                    .collect(),
                game_versions: v
                    .game_versions
                    .unwrap_or_default()
                    .split(',')
                    .map(|x| x.to_string())
                    .collect(),
                loaders: v
                    .loaders
                    .unwrap_or_default()
                    .split(',')
                    .map(|x| x.to_string())
                    .collect(),
                featured: v.featured,
                dependencies: v
                    .dependencies
                    .unwrap_or_default()
                    .split(" ,")
                    .map(|f| {
                        let dependency: Vec<&str> = f.split(", ").collect();

                        if dependency.len() >= 3 {
                            Some(QueryDependency {
                                project_id: match &*dependency[1] {
                                    "0" => None,
                                    _ => match dependency[1].parse() {
                                        Ok(x) => Some(ProjectId(x)),
                                        Err(_) => None,
                                    },
                                },
                                version_id: match &*dependency[0] {
                                    "0" => None,
                                    _ => match dependency[0].parse() {
                                        Ok(x) => Some(VersionId(x)),
                                        Err(_) => None,
                                    },
                                },
                                dependency_type: dependency[2].to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .flatten()
                    .collect(),
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn get_many_full<'a, E>(
        version_ids: Vec<VersionId>,
        exec: E,
    ) -> Result<Vec<QueryVersion>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Copy,
    {
        use futures::stream::TryStreamExt;

        let version_ids_parsed: Vec<i64> = version_ids.into_iter().map(|x| x.0).collect();
        sqlx::query!(
            "
            SELECT v.id id, v.mod_id mod_id, v.author_id author_id, v.name version_name, v.version_number version_number,
            v.changelog changelog, v.changelog_url changelog_url, v.date_published date_published, v.downloads downloads,
            rc.channel release_channel, v.featured featured,
            STRING_AGG(DISTINCT gv.version, ',') game_versions, STRING_AGG(DISTINCT l.loader, ',') loaders,
            STRING_AGG(DISTINCT f.id || ', ' || f.filename || ', ' || f.is_primary || ', ' || f.url, ' ,') files,
            STRING_AGG(DISTINCT h.algorithm || ', ' || encode(h.hash, 'escape') || ', ' || h.file_id,  ' ,') hashes,
            STRING_AGG(DISTINCT COALESCE(d.dependency_id, 0) || ', ' || COALESCE(d.mod_dependency_id, 0) || ', ' || d.dependency_type,  ' ,') dependencies
            FROM versions v
            INNER JOIN release_channels rc on v.release_channel = rc.id
            LEFT OUTER JOIN game_versions_versions gvv on v.id = gvv.joining_version_id
            LEFT OUTER JOIN game_versions gv on gvv.game_version_id = gv.id
            LEFT OUTER JOIN loaders_versions lv on v.id = lv.version_id
            LEFT OUTER JOIN loaders l on lv.loader_id = l.id
            LEFT OUTER JOIN files f on v.id = f.version_id
            LEFT OUTER JOIN hashes h on f.id = h.file_id
            LEFT OUTER JOIN dependencies d on v.id = d.dependent_id
            WHERE v.id IN (SELECT * FROM UNNEST($1::bigint[]))
            GROUP BY rc.id, v.id
            ORDER BY v.date_published ASC;
            ",
            &version_ids_parsed
        )
            .fetch_many(exec)
            .try_filter_map(|e| async {
                Ok(e.right().map(|v| {
                    let hashes: Vec<(FileId, String, Vec<u8>)> = v.hashes.unwrap_or_default().split(" ,").map(|f| {
                        let hash: Vec<&str> = f.split(", ").collect();

                        if hash.len() >= 3 {
                            Some((
                                FileId(hash[2].parse().unwrap_or(0)),
                                hash[0].to_string(),
                                hash[1].to_string().into_bytes(),
                            ))
                        } else {
                            None
                        }
                    }).flatten().collect();

                    QueryVersion {
                        id: VersionId(v.id),
                        project_id: ProjectId(v.mod_id),
                        author_id: UserId(v.author_id),
                        name: v.version_name,
                        version_number: v.version_number,
                        changelog: v.changelog,
                        changelog_url: v.changelog_url,
                        date_published: v.date_published,
                        downloads: v.downloads,
                        release_channel: v.release_channel,
                        files: v.files.unwrap_or_default().split(" ,").map(|f| {
                            let file: Vec<&str> = f.split(", ").collect();

                            if file.len() >= 4 {
                                let file_id = FileId(file[0].parse().unwrap_or(0));
                                let mut file_hashes = HashMap::new();

                                for hash in &hashes {
                                    if (hash.0).0 == file_id.0 {
                                        file_hashes.insert(hash.1.clone(), hash.2.clone());
                                    }
                                }

                                Some(QueryFile {
                                    id: file_id,
                                    url: file[3].to_string(),
                                    filename: file[1].to_string(),
                                    hashes: file_hashes,
                                    primary: file[2].parse().unwrap_or(false),
                                })
                            } else {
                                None
                            }
                        }).flatten().collect(),
                        game_versions: v.game_versions.unwrap_or_default().split(',').map(|x| x.to_string()).collect(),
                        loaders: v.loaders.unwrap_or_default().split(',').map(|x| x.to_string()).collect(),
                        featured: v.featured,
                        dependencies: v.dependencies
                            .unwrap_or_default()
                            .split(" ,")
                            .map(|f| {
                                let dependency: Vec<&str> = f.split(", ").collect();

                                if dependency.len() >= 3 {
                                    Some(QueryDependency {
                                        project_id: match &*dependency[1] {
                                            "0" => None,
                                            _ => match dependency[1].parse() {
                                                Ok(x) => Some(ProjectId(x)),
                                                Err(_) => None,
                                            },
                                        },
                                        version_id: match &*dependency[0] {
                                            "0" => None,
                                            _ => match dependency[0].parse() {
                                                Ok(x) => Some(VersionId(x)),
                                                Err(_) => None,
                                            },
                                        },
                                        dependency_type: dependency[2].to_string(),
                                    })
                                } else {
                                    None
                                }
                            }).flatten().collect(),
                    }
                }))
            })
            .try_collect::<Vec<QueryVersion>>()
            .await
    }
}

pub struct ReleaseChannel {
    pub id: ChannelId,
    pub channel: String,
}

pub struct VersionFile {
    pub id: FileId,
    pub version_id: VersionId,
    pub url: String,
    pub filename: String,
    pub primary: bool,
}

pub struct FileHash {
    pub file_id: FileId,
    pub algorithm: String,
    pub hash: Vec<u8>,
}

#[derive(Clone)]
pub struct QueryVersion {
    pub id: VersionId,
    pub project_id: ProjectId,
    pub author_id: UserId,
    pub name: String,
    pub version_number: String,
    pub changelog: String,
    pub changelog_url: Option<String>,
    pub date_published: chrono::DateTime<chrono::Utc>,
    pub downloads: i32,

    pub release_channel: String,
    pub files: Vec<QueryFile>,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub featured: bool,
    pub dependencies: Vec<QueryDependency>,
}

#[derive(Clone)]
pub struct QueryDependency {
    pub project_id: Option<ProjectId>,
    pub version_id: Option<VersionId>,
    pub dependency_type: String,
}

#[derive(Clone)]
pub struct QueryFile {
    pub id: FileId,
    pub url: String,
    pub filename: String,
    pub hashes: HashMap<String, Vec<u8>>,
    pub primary: bool,
}
