"""Writes the record of an upload: `pixi run record-upload <link>`.

Run the command after you upload the video of the present transcript to
YouTube, with the link of the upload. It writes `upload.json`: the id of the
video, and the hash of the content of the transcript.
"""

import argparse

from tools import paths, upload


def main() -> None:
    parser = argparse.ArgumentParser(description="Record the upload of the concepts video to YouTube.")
    parser.add_argument("link", help="the link of the video on YouTube, for example https://youtu.be/<id>")
    try:
        youtube_id = upload.youtube_id_of(parser.parse_args().link)
    except upload.UploadError as not_a_link:
        raise SystemExit(str(not_a_link)) from not_a_link
    record = upload.Upload(youtube_id=youtube_id, content_sha256=upload.content_sha256(paths.TRANSCRIPT))
    upload.write_upload(paths.UPLOAD, record)
    print(f"{paths.UPLOAD.name} records the video `{youtube_id}` as the video of this transcript.")


if __name__ == "__main__":
    main()
