use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::compiler;

#[derive(Parser)]
#[command(
    version,
    about = "Compile heterogeneous sources into backend-neutral evidence packages"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a frozen source assignment into an immutable evidence package.
    Compile {
        #[arg(long)]
        assignments: PathBuf,
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        checksums: PathBuf,
        /// Deterministic manifest produced by `materialize-email-attachments`; repeat per MBOX.
        #[arg(long = "email-attachment-manifest")]
        email_attachment_manifests: Vec<PathBuf>,
        /// Supplied or resolved output path components ending case-insensitively in `.lock`, `.staging`, or `.staging.owner.json` are reserved.
        #[arg(long)]
        output: PathBuf,
    },
    /// Decode Gmail MBOX attachments into owner-private content-addressed artifacts.
    MaterializeEmailAttachments {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        artifact_dir: PathBuf,
        /// Supplied or resolved output path components ending case-insensitively in `.lock`, `.staging`, or `.staging.owner.json` are reserved.
        #[arg(long)]
        output_manifest: PathBuf,
    },
    /// Validate every binding and checksum in an evidence package.
    Validate {
        #[arg(long, value_name = "DIRECTORY")]
        package: PathBuf,
    },
    /// Emit a compact JSON inventory of a validated evidence package.
    Inspect {
        #[arg(long, value_name = "DIRECTORY")]
        package: PathBuf,
    },
    /// Inventory RFC 4180 conversation tables into a frozen source assignment.
    InventoryConversationTables {
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long = "file", required = true)]
        files: Vec<PathBuf>,
        /// Optional CSV selecting `conversation_id` values by source-file community stem.
        #[arg(long)]
        selection_table: Option<PathBuf>,
        /// Supplied or resolved output path components ending case-insensitively in `.lock`, `.staging`, or `.staging.owner.json` are reserved.
        #[arg(long)]
        output: PathBuf,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Compile {
            assignments,
            source_root,
            checksums,
            email_attachment_manifests,
            output,
        } => compiler::compile_with_email_attachments(
            &assignments,
            &source_root,
            &checksums,
            &email_attachment_manifests,
            &output,
        ),
        Command::MaterializeEmailAttachments {
            source_root,
            file,
            artifact_dir,
            output_manifest,
        } => compiler::materialize_email_attachments(
            &source_root,
            &file,
            &artifact_dir,
            &output_manifest,
        ),
        Command::Validate { package } => compiler::validate(&package),
        Command::Inspect { package } => compiler::inspect(&package),
        Command::InventoryConversationTables {
            source_root,
            files,
            selection_table,
            output,
        } => compiler::inventory_conversation_tables(
            &source_root,
            &files,
            selection_table.as_deref(),
            &output,
        ),
    }
}
