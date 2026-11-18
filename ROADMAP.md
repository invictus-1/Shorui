# Roadmap

Planned work, not promises. Nothing here is started unless it says so. Sizes and speeds are estimates until measured.

## OCR

Shipped in 0.4.0: scanned pages are read with the OCR engine built into Windows, so find, select, copy and annotations work on them. It is offline and adds nothing to the download, but it is weaker on skewed, low-resolution or handwritten scans, and it only reads languages installed in Windows.

### Neural OCR as an option

Goal: a second, more accurate engine the user can switch to in Settings, without making the default install bigger or slower.

- **Models:** a detection model plus a recognition model in the PP-OCR "mobile" class, run through ONNX Runtime. These are small (roughly 10 to 20 MB for one language family) and much better than the Windows engine on poor scans and dense layouts.
- **Not bundled:** the models and the runtime are downloaded the first time the user turns the option on, from this repository's releases, and verified by checksum. The installer stays near its current size. This would be the second network request Shorui ever makes, so it must be explicit and opt-in.
- **Optimised:**
  - Quantised (int8) models for CPU.
  - DirectML on the GPU when one is available, CPU otherwise.
  - Detect once per page, then recognise all text lines in batches.
  - Render the page once at the resolution the model wants and reuse it.
  - Same priority as today: behind tile rendering, one page per turn, cancellable.
- **Fits the existing design:** any engine only has to return characters and boxes. `src/ocr.rs` becomes a trait with two implementations; nothing in find, selection or annotations changes.
- **Per-language packs:** Latin first, then Japanese and Chinese as separate recognition models.
- **Open questions:** licence of each model, real accuracy against the Windows engine on a test set of scans, and cold-start time of the runtime.

### Around OCR

- **Searchable PDF export:** write the recognised text into the file as an invisible text layer, so any reader can search it.
- **Disk cache:** keep recognised text between sessions, keyed by file hash, so a long scan is read once ever, not once per open.
- **Progress:** show how much of a scanned document has been read while a search is running.
- **Deskew** pages before recognition.
- **Per-character boxes:** the Windows engine gives one box per word; letters are spaced evenly inside it. Selection inside a word is therefore approximate.

## Editing and annotations

- Shapes, arrows and text boxes.
- Pen colour and width; an eraser.
- Edit and move existing notes.
- Fill in PDF forms.
- Redaction that removes the content, not just covers it.
- Crop pages; add blank pages.
- Undo for delete, insert and remove-annotation without a whole-document snapshot.

## Reading

- Remember page and zoom for every recent file, not only the last session.
- Search options: whole word, match case.
- Thumbnails for the outline targets; a search results list.
- Presentation mode.
- Invert colours for night reading.
- Text-to-speech using the Windows voices.

## Platform

- Code signing, so SmartScreen stops warning on the installer.
- Verify downloaded updates against a signature before running them.
- Automated tests in CI on every push, built from the scripted self-test.
- Test against a corpus of large and awkward real-world PDFs.
- Portable build: one folder, settings beside the exe.
