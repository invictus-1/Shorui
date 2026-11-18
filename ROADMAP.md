# Roadmap

Planned work, not promises. Nothing here is started unless it says so. Sizes and speeds are estimates until measured.

## OCR

Shipped in 0.4.0: scanned pages are read with the OCR engine built into Windows.

Shipped in 0.5.0:

- **Neural OCR as an option.** PP-OCRv5 mobile detection and recognition, through the DirectML build of ONNX Runtime 1.24: on the graphics card when there is one, on the processor otherwise. Downloaded on request from the `ocr-models-1` release and checked against SHA-256 hashes in the code. Latin and Chinese/Japanese recognisers. On five bad test scans it read 94% of characters against the Windows engine's 58%.
- **OCR on its own thread**, so a slow page never holds up rendering. Visible pages first, then search, one page at a time.
- **Disk cache** of recognised text, keyed by the file's contents.
- **Progress**: find says which page OCR has reached.
- **Straightening**: each detected line is read along its own direction, which handles skew up to about 45° without a separate deskew step.
- **Per-character boxes** from the neural engine, from where CTC decoding saw each character.

Measured along the way, and why some planned items changed:

- Dynamic int8 quantisation made recognition slower on the processor (ONNX Runtime's integer convolutions) and wrecked detection. The models ship as fp32. Static (QDQ) quantisation with a calibration set might still pay off.
- DirectML is in maintenance at Microsoft. If it stops being updated, the replacement is Windows ML, which brings ONNX Runtime and its execution providers with Windows itself.

Still to do:

- **Searchable PDF export**: write the recognised text into the file as an invisible text layer, so any reader can search it. Latin text can use a standard font; Chinese and Japanese need an embedded font subset.
- **Very poor scans**: on the worst test page (105 dpi, heavy noise and blur) the Latin model reads 79% and the Chinese/Japanese model almost nothing. Try upscaling small text before recognition, and the larger "server" models as a third, opt-in pack.
- **More language packs**: Korean, Cyrillic, Greek, Arabic, Devanagari exist as PP-OCRv5 mobile models.
- **Reading order** for multi-column pages: lines are ordered top to bottom across the whole page, so columns interleave in copied text.
- **Handwriting**: neither engine is good at it.

## Editing and annotations

- Shapes, arrows and text boxes.
- Pen colour and width; an eraser.
- Edit and move existing notes.
- Fill in PDF forms.
- Redaction that removes the content, not just covers it.
- Crop pages; add blank pages.
- Undo for delete, insert and remove-annotation without a whole-document snapshot.

## Reading

Shipped in 0.5.0: page and zoom remembered for every recent file, a start screen with covers and pinning, reopen closed tab, match case and whole-word search, a results list, presentation mode, and night reading.

- Thumbnails for the outline targets.
- Text-to-speech using the Windows voices.
- A recent-files jump list on the taskbar icon.
- Night reading that leaves photographs alone and only flips text and line art.

## Platform

- Code signing, so SmartScreen stops warning on the installer.
- Verify downloaded updates against a signature before running them.
- Automated tests in CI on every push, built from the scripted self-test.
- Test against a corpus of large and awkward real-world PDFs.
- Portable build: one folder, settings beside the exe.
