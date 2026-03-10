import os
import xxhash  # Changed from hashlib
import pandas as pd
from tqdm import tqdm
from loguru import logger

CHECKPOINT_INTERVAL = 500
CHECKPOINT_DIR = 'checkpoints'
ALL_FILES_CSV = 'all_files.csv'
DEDUP_CSV = 'deduplicated_files.csv'
DELETED_CSV = 'deleted_files.csv'

def get_xxhash(file_path, chunk_size=8192):
    h = xxhash.xxh64()  # 64-bit hash
    try:
        with open(file_path, "rb") as f:
            for chunk in iter(lambda: f.read(chunk_size), b""):
                h.update(chunk)
    except Exception as e:
        logger.warning(f"Failed to read {file_path}: {e}")
        return None
    return h.hexdigest()

def save_checkpoint_batch(batch_df, batch_index):
    os.makedirs(CHECKPOINT_DIR, exist_ok=True)
    path = os.path.join(CHECKPOINT_DIR, f"checkpoint_{batch_index}.csv")
    batch_df.to_csv(path, index=False)
    logger.info(f"Saved checkpoint {path} with {len(batch_df)} files")

def process_folder(root_dir):
    all_files = []
    for root, dirs, files in os.walk(root_dir):
        for name in files:
            all_files.append(os.path.join(root, name))

    batch = []
    batch_index = 0

    for i, path in enumerate(tqdm(all_files, desc="Scanning files")):
        try:
            size = os.path.getsize(path)
            created = os.path.getctime(path)
            file_hash = get_xxhash(path)
            if file_hash:
                batch.append((path, size, file_hash, created))
        except Exception as e:
            logger.warning(f"Error processing {path}: {e}")
            continue

        if len(batch) == CHECKPOINT_INTERVAL:
            df = pd.DataFrame(batch, columns=["file_path", "file_size", "xxhash", "created_time"])
            save_checkpoint_batch(df, batch_index)
            batch = []
            batch_index += 1

    if batch:
        df = pd.DataFrame(batch, columns=["file_path", "file_size", "xxhash", "created_time"])
        save_checkpoint_batch(df, batch_index)

    # Combine all checkpoints
    all_dfs = []
    for file in sorted(os.listdir(CHECKPOINT_DIR)):
        if file.startswith("checkpoint_") and file.endswith(".csv"):
            df = pd.read_csv(os.path.join(CHECKPOINT_DIR, file))
            all_dfs.append(df)

    final_df = pd.concat(all_dfs, ignore_index=True)
    final_df["file_name"] = final_df["file_path"].apply(os.path.basename)
    final_df.to_csv(ALL_FILES_CSV, index=False)
    return final_df

def delete_duplicates(df):
    df = df.sort_values("file_size", ascending=False)

    dedup_rows = []
    deleted_rows = []

    grouped = df.groupby("xxhash")

    total_duplicates = 0
    total_kept = 0
    total_deleted = 0

    for _, group in grouped:
        if len(group) == 1:
            dedup_rows.append(group)
            continue

        total_duplicates += len(group)
        same_name_group = group[group["file_name"] == group.iloc[0]["file_name"]]
        group_sorted = same_name_group if len(same_name_group) > 1 else group
        group_sorted = group_sorted.sort_values("created_time")

        keep_row = group_sorted.iloc[:1]
        drop_rows = group_sorted.iloc[1:]

        dedup_rows.append(keep_row)
        deleted_rows.append(drop_rows)

        total_kept += 1
        total_deleted += len(drop_rows)

    dedup_df = pd.concat(dedup_rows)
    deleted_df = pd.concat(deleted_rows)

    removed_count = 0
    for _, row in deleted_df.iterrows():
        try:
            os.remove(row["file_path"])
            removed_count += 1
        except Exception as e:
            logger.warning(f"Could not delete {row['file_path']}: {e}")

    dedup_df.to_csv(DEDUP_CSV, index=False)
    deleted_df.to_csv(DELETED_CSV, index=False)

    logger.info("Summary:")
    logger.info(f"  Total duplicate filesets found: {total_duplicates}")
    logger.info(f"  Files kept: {total_kept}")
    logger.info(f"  Files deleted: {total_deleted}")
    logger.info(f"  Files successfully removed: {removed_count}")

    return removed_count, dedup_df, deleted_df

if __name__ == "__main__":
    root_folder = input("Enter the root folder path to scan: ").strip()
    df_all = process_folder(root_folder)

    removed_count, df_kept, df_deleted = delete_duplicates(df_all)

    logger.info(f"Total files scanned: {len(df_all)}")
    logger.info(f"Files kept: {len(df_kept)}")
    logger.info(f"Files deleted: {removed_count}")
    logger.info("Outputs saved to all_files.csv, deduplicated_files.csv, deleted_files.csv")
