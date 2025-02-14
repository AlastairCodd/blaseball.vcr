use blaseball_vcr::encoder::*;
use blaseball_vcr::*;
use progress_bar::color::{Color, Style};
use progress_bar::progress_bar::ProgressBar;
use serde::Serialize;
use serde_json::Value as JSONValue;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, Write};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChroniclerParameters {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "page")]
    next_page: Option<String>,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<String>,
    count: u32,
}

async fn paged_get(
    client: &reqwest::Client,
    progress: &mut ProgressBar,
    show_progress: bool,
    url: &str,
    mut parameters: ChroniclerParameters,
) -> VCRResult<Vec<ChroniclerEntity>> {
    let mut results: Vec<ChroniclerEntity> = Vec::new();

    let mut page = 1;

    loop {
        if show_progress {
            progress.print_info(
                "fetching",
                &format!(
                    "page #{} - {} total entities",
                    page,
                    page * parameters.count
                ),
                Color::Red,
                Style::Italic,
            );
        }

        let mut chron_response: ChroniclerResponse<ChroniclerEntity> = client
            .get(url)
            .query(&parameters)
            .send()
            .await?
            .json()
            .await?;
        results.append(&mut chron_response.items);

        if let Some(next_page) = chron_response.next_page {
            parameters.next_page = Some(next_page);
            page += 1;
        } else {
            break;
        }
    }

    Ok(results)
}

#[tokio::main]
pub async fn main() -> VCRResult<()> {
    let client = reqwest::Client::new();
    let mut entity_types: Vec<String> = env::args().skip(1).collect();
    let checkpoint_every = entity_types.remove(0).parse::<u16>().unwrap_or(u16::MAX);
    let dict_path = entity_types.remove(0);

    let mut patch_compressor = if dict_path == "nodict" {
        zstd::block::Compressor::new()
    } else {
        let mut dict_f = File::open(&dict_path).map_err(VCRError::IOError)?;
        let mut dict: Vec<u8> = Vec::new();
        dict_f.read_to_end(&mut dict).map_err(VCRError::IOError)?;
        zstd::block::Compressor::with_dict(dict)
    };

    for etype in entity_types {
        let mut progress_bar = ProgressBar::new(0);
        println!("-> Fetching list of entities of type {}", etype);
        let entity_ids: Vec<String> = paged_get(
            &client,
            &mut progress_bar,
            false,
            "https://api.sibr.dev/chronicler/v2/entities",
            ChroniclerParameters {
                next_page: None,
                entity_type: etype.to_owned(),
                id: None,
                order: None,
                count: 1000,
            },
        )
        .await?
        .into_iter()
        .map(|e| e.entity_id)
        .collect();

        let mut entity_lookup_table: HashMap<String, EntityData> = HashMap::new();

        println!("| found {} entities", entity_ids.len());
        let mut progress_bar = ProgressBar::new(entity_ids.len());
        progress_bar.set_action(
            "Loading & encoding entity versions",
            Color::Blue,
            Style::Bold,
        );

        let out_file =
            File::create(&format!("./tapes/{}.riv", etype)).map_err(VCRError::IOError)?;
        let mut out = BufWriter::new(out_file);

        for id in entity_ids {
            progress_bar.set_action(&id, Color::Green, Style::Bold);

            progress_bar.print_info(
                "downloading",
                &format!("entity {} of type {}", id, etype),
                Color::Green,
                Style::Italic,
            );

            let mut entity_versions: Vec<(u32, JSONValue)> = paged_get(
                &client,
                &mut progress_bar,
                true,
                "https://api.sibr.dev/chronicler/v2/versions",
                ChroniclerParameters {
                    next_page: None,
                    entity_type: etype.to_owned(),
                    id: Some(id.clone()),
                    order: Some("asc".to_owned()),
                    count: 1000,
                },
            )
            .await?
            .into_iter()
            .map(|e| (e.valid_from.timestamp() as u32, e.data))
            .collect();

            entity_versions.sort_by_key(|v| v.0);

            let (patches, path_map, baseval) = encode(entity_versions, checkpoint_every);

            let mut offsets: Vec<(u32, u32, u32)> = Vec::new(); // timestamp:start_position:end_position

            for (time, patch) in patches {
                let start_pos = out.stream_position().map_err(VCRError::IOError)?;

                let patch_bytes = patch.concat();
                out.write_all(&patch_compressor.compress(&patch_bytes, 22).unwrap())
                    .unwrap();

                let end_pos = out.stream_position().map_err(VCRError::IOError)?;
                offsets.push((time, start_pos as u32, end_pos as u32));
            }

            entity_lookup_table.insert(
                id.to_owned(),
                EntityData {
                    patches: offsets,
                    path_map,
                    checkpoint_every,
                    base: baseval,
                },
            );

            progress_bar.inc();

            out.flush().map_err(VCRError::IOError)?;
        }

        progress_bar.finalize();

        let entity_table_f = File::create(&format!("./tapes/{}.header.riv.zstd", etype))
            .map_err(VCRError::IOError)?;
        let mut entity_table_compressor = zstd::Encoder::new(entity_table_f, 21).unwrap();
        entity_table_compressor
            .long_distance_matching(true)
            .unwrap();
        entity_table_compressor
            .write_all(
                &rmp_serde::to_vec(&entity_lookup_table)
                    .map_err(VCRError::MsgPackEncError)
                    .unwrap(),
            )
            .map_err(VCRError::IOError)
            .unwrap();
        entity_table_compressor.finish().unwrap();
        out.get_mut().sync_all().map_err(VCRError::IOError)?;
    }

    Ok(())
}
