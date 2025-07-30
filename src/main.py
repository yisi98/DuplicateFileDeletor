import os
import hashlib
import pandas as pd
from tqdm import tqdm
from loguru import logger  # NEW

CHECKPOINT_INTERVAL = 500
CHECKPOINT_DIR = 'checkpoints'
ALL_FILES_CSV = 'all_files.csv'
DEDUP_CSV = 'deduplicated_files.csv'
DELETED_CSV = 'deleted_files.csv'

def get_md5(file_path, chunk_size=8192):
    hash_md5 = hashlib.md5()
    try:
        with open(file_path, "rb") as f:
            for chunk in iter(lambda: f.read(chunk_size), b""):
                hash_md5.update(chunk)
    except Exception as e:
        logger.warning(f"Failed to read {file_path}: {e}")
        return None
    return hash_md5.hexdigest()

def save_checkpoint(df, index):
    os.makedirs(CHECKPOINT_DIR, exist_ok=True)
    df.to_csv(f"{CHECKPOINT_DIR}/checkpoint_{index}.csv", index=False)

def load_last_checkpoint():
    if not os.path.exists(CHECKPOINT_DIR):
        return pd.DataFrame(columns=["file_path", "file_size", "md5", "created_time"]), 0

    checkpoints = sorted([
        f for f in os.listdir(CHECKPOINT_DIR)
        if f.startswith("checkpoint_") and f.endswith(".csv")
    ], key=lambda x: int(x.split('_')[1].split('.')[0]))

    if not checkpoints:
        return pd.DataFrame(columns=["file_path", "file_size", "md5", "created_time"]), 0

    last_checkpoint = checkpoints[-1]
    df = pd.read_csv(os.path.join(CHECKPOINT_DIR, last_checkpoint))
    return df, len(df)

def process_folder(root_dir):
    all_files = []
    for root, dirs, files in os.walk(root_dir):
        for name in files:
            all_files.append(os.path.join(root, name))

    df, start_idx = load_last_checkpoint()
    logger.info(f"Resuming from file {start_idx} of {len(all_files)}")

    for i in tqdm(range(start_idx, len(all_files)), desc="Scanning files"):
        path = all_files[i]
        try:
            size = os.path.getsize(path)
            md5 = get_md5(path)
            created = os.path.getctime(path)
            df.loc[len(df)] = [path, size, md5, created]
        except:
            continue

        if (i + 1) % CHECKPOINT_INTERVAL == 0 or i == len(all_files) - 1:
            save_checkpoint(df, len(df))
            tqdm.write(f"Checkpoint saved at {len(df)} files")

    df.columns = ["file_path", "file_size", "md5", "created_time"]
    df["file_name"] = df["file_path"].apply(os.path.basename)
    df.to_csv(ALL_FILES_CSV, index=False)
    return df

def delete_duplicates(df):
    df = df.sort_values("file_size", ascending=False)

    dedup_rows = []
    deleted_rows = []

    grouped = df.groupby("md5")

    total_duplicates = 0
    total_kept = 0
    total_deleted = 0

    for md5, group in grouped:
        if len(group) == 1:
            dedup_rows.append(group)
            continue

        total_duplicates += len(group)

        same_name_group = group[group["file_name"] == group.iloc[0]["file_name"]]
        if len(same_name_group) > 1:
            group_sorted = same_name_group.sort_values("created_time")
        else:
            group_sorted = group.sort_values("created_time")

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
