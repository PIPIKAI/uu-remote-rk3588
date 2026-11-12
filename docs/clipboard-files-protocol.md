# File clipboard protocol investigation

Bidirectional file and folder copying between the official Windows controller and Ubuntu GNOME
Wayland host was manually accepted by the user on 2026-11-12, including the folder-content fix.
The bidirectional file bridge is enabled only by `UU_CLIPBOARD_FILES=1`. The reverse probe has been removed.
Do not advertise additional clipboard feature versions yet.

Observed with an official Windows controller, 2026-11-12. Logs contain only format names/IDs,
field numbers, sizes, counts and correlation metadata, never file names, paths or contents.

## Wire evidence

Clipboard RPCs use request case 9 and response case 5 of the existing RPC envelope, on TEXT.
A file copy advertises the named formats `FileGroupDescriptorW` and `FileContents`.
Their registered numeric IDs must be taken from the offer.

The text GetData operation (ClipboardRequest case 2) does **not** retrieve this controller's
file descriptor: it replies ClipboardResponse case 2 with only field 1 = 2, without a data key
or blocks. Correlate this failure through the RPC request header; an empty data key must not
silently hide it.

To observe the official consumer, a probe advertises those same file formats back to Windows.
Pasting there produces ClipboardRequest **case 4**, with one varint at field 1. Its value is a
32-bit correlation ID, rather than either registered format ID. Replaying the exact operation
against a fresh, locally copied source yields:

- ClipboardResponse case 4: field 1 echoes the correlation ID, field 2 = 1, field 3 = 1.
- ClipboardRequest case 7: field 1 echoes that ID, field 2 = 1, field 3 is a 21-byte payload
  for the small file used in this experiment.

When the source clipboard has already been replaced by the probe offer, the same case 4
request instead returns fields 1 (echo) and 3 = 2, without field 2 or a case 7 block.
This distinguishes valid source ownership from a failed query. Field 2 of the successful
response appears to count blocks; field 2 of the block appears to be a one-based block index.
The case 7 payload is one protobuf file entry: field 1 UTF-8 name, field 2 Windows attributes
(32 for the observed regular file), field 3 modification timestamp, field 4 byte size. A case 7
response acknowledges fields 1 (same ID), 2 (same one-based entry index), 3 (status 1).
It is not a raw Windows FILEGROUPDESCRIPTORW.

Pasting the real list into Windows produced ClipboardRequest case 5 with fields 1 (same ID),
3 = 2 (range read), and 5 (requested byte count, equal to the file size). Fields 2 (file index)
and 4 (offset) were absent/zero. A direct request to the Windows source succeeded with
ClipboardResponse case 5: field 1 same ID, field 2 **inline bytes**, field 3 status 1.
There is no separate content-block request in this observed small-file path.

## Virtual file implementation

The old URI-transfer trigger was insufficient: GNOME reads copied-files metadata in the background,
so that event caused premature downloads. It also rejects a trailing empty line in
`x-special/gnome-copied-files`; emit `copy\nURI` without a final newline.

The replacement publishes an owner-only, read-only FUSE tree. Lookup, getattr, open and clipboard
URI serialization do not fetch content or create cache files. FUSE content reads request ranges
and write only validated response bytes into private cache files. Each range is at most 128 KiB;
metadata allows up to 1 GiB total and 10,000 entries. File handles close on release. Replaced
selections invalidate pending requests, release cached data and prune unused virtual entries.
Shutdown unmounts the private filesystem; startup recovers its orphaned per-process mounts.

File index is encoded in case 5 field 2, and nonzero position in field 4. These extend the observed
zero-index/zero-position request. Large and small inbound files and bidirectional folder copies
have passed manual user acceptance between Windows and Ubuntu GNOME Wayland.

## Intended behavior

Copy synchronizes file metadata. A file-manager clipboard read for paste should trigger
content transfer, including right-click paste. The user chooses the destination in the file
manager; it is never compiled into the host. Received content is staged privately and exposed
through Portal `text/uri-list` / `x-special/gnome-copied-files`. Selections may contain files and folders, bounded to 1 GiB total and 10,000 entries.
Cut/delete semantics and changes to independent desktop/tool peers are outside this path.

## Folder parsing and outbound provider (2026-11-12)

Read-only inspection of the official Windows 4.46.0.4716 host from the official download
(`GameViewerServer.exe` SHA-256 `91cfe3bfc5582776a56a2cf6294c45afb7d65d68abe8c4a483e99a8549b00b5b`)
found its embedded `clipboard.proto` descriptor. `ClipboardFileDescriptorSegment` has
field 1 task_id, field 2 segment_id and **repeated message field 3 file_descs**. The old
receiver read only the first occurrence, dropping the children after a directory entry.
All occurrences are now retained in segment order, preserving list_index including directories.

The descriptor confirms case 4 response field 2 is segment_count. Case 5 request fields are
1 task_id, 2 list_index, 3 flags, 4 pos_offset, 5 requested_len; response fields are
1 task_id, 2 data, 3 err, 4 pos_offset, 5 list_index. Case 6 cancels task_id and replies
with task_id at field 1 and err at field 2. The outbound provider uses these same shapes,
size/range flags from Windows FILECONTENTS semantics, and one-based segment IDs as observed above.

Portal prefers local file URI formats over file-manager text representations. Separate polling
preserves text events and file events; ownership changes invalidate the previous local offer.
The sender gathers metadata recursively without reading file bodies, rejects links and special
files, validates file identity/size/mtime before range reads, and uses the existing selection limits.
Files, directories, nested directories and multiple roots are offered in enumeration order.
Reverse transfers and folder completeness passed manual user acceptance between Windows and
Ubuntu GNOME Wayland. No regression tests were run for this fix.
