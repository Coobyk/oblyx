import { convertFile, formatSize, prewarm, loadThumbnails } from "./engine.js"

const input = document.querySelector("#notebook-file")
const dropzone = document.querySelector("#dropzone")
const fileRow = document.querySelector("#file-row")
const fileName = document.querySelector("#file-name")
const fileSize = document.querySelector("#file-size")
const options = document.querySelector("#options")
const packageOption = document.querySelector("#package-option")
const formatOptions = document.querySelector("#format-options")
const packageOptions = document.querySelector("#package-options")
const convertButton = document.querySelector("#convert")
const changeButton = document.querySelector("#change-file")
const progressArea = document.querySelector("#progress-area")
const progressLabel = document.querySelector("#progress-label")
const progressPercent = document.querySelector("#progress-percent")
const progressTrack = document.querySelector(".progress-track")
const progressValue = document.querySelector("#progress-value")
const errorMessage = document.querySelector("#error")
const results = document.querySelector("#results")
const thumbsEl = document.querySelector("#thumbs")
const previewButton = document.querySelector("#preview-toggle")
const thumbsSection = document.querySelector("#thumbs-section")

let selectedFile = null
let preloadedBytes = null
let outputFiles = []
let outputThumbs = []
let loadMoreThumbs = null
let loadingMore = false
let busy = false
let outputFormat = "pdf"
let outputPackaging = "zip"
let savedMeta = null
let previewLoaded = false

function selectOption(group, value) {
  for (const button of group.querySelectorAll(".segment")) {
    const selected = button.dataset.value === value
    button.classList.toggle("is-active", selected)
    button.setAttribute("aria-pressed", String(selected))
  }
}

function clearOutputs() {
  for (const file of outputFiles) URL.revokeObjectURL(file.url)
  for (const thumb of outputThumbs) URL.revokeObjectURL(thumb.url)
  outputFiles = []
  outputThumbs = []
  loadMoreThumbs = null
  savedMeta = null
  previewLoaded = false
  results.replaceChildren()
  results.hidden = true
  if (thumbsEl) {
    thumbsEl.replaceChildren()
    thumbsEl.hidden = true
  }
  if (thumbsSection) thumbsSection.hidden = true
  if (previewButton) {
    previewButton.hidden = true
    previewButton.disabled = false
    previewButton.textContent = "Show Preview"
  }
}

function appendThumbs(thumbs) {
  for (const thumb of thumbs) {
    const img = document.createElement("img")
    img.src = thumb.url
    img.alt = thumb.label
    img.loading = "lazy"
    const cap = document.createElement("figcaption")
    cap.textContent = thumb.label
    const cell = document.createElement("figure")
    cell.className = "thumb"
    cell.append(img, cap)
    thumbsEl.append(cell)
  }
}

function showError(message = "") {
  errorMessage.textContent = message
  errorMessage.hidden = !message
}

function reset() {
  clearOutputs()
  selectedFile = null
  preloadedBytes = null
  input.value = ""
  outputFormat = "pdf"
  outputPackaging = "zip"
  selectOption(formatOptions, outputFormat)
  selectOption(packageOptions, outputPackaging)
  // Show all format options again
  const gnButton = formatOptions.querySelector('[data-value="goodnotes"]')
  const ntbButton = formatOptions.querySelector('[data-value="ntb"]')
  if (gnButton) gnButton.hidden = false
  if (ntbButton) ntbButton.hidden = false
  fileRow.hidden = true
  options.hidden = true
  packageOption.hidden = true
  progressArea.hidden = true
  convertButton.hidden = false
  convertButton.disabled = false
  input.disabled = false
  changeButton.disabled = false
  for (const button of document.querySelectorAll(".segment")) button.disabled = false
  dropzone.hidden = false
  showError()
}

function selectFile(file) {
  if (!file) return
  const isZip = /\.zip$/i.test(file.name)
  const isGoodnotes = /\.goodnotes$/i.test(file.name)
  const isNtb = /\.ntb$/i.test(file.name)
  if (!isZip && !isGoodnotes && !isNtb) {
    showError("Choose a .goodnotes, .ntb file, or .zip archive.")
    return
  }

  clearOutputs()
  showError()
  selectedFile = file
  fileName.textContent = file.name
  const kind = isZip ? "ZIP archive" : isNtb ? "Notability notebook" : "Goodnotes notebook"
  fileSize.textContent = `${formatSize(file.size)} · ${kind}`
  dropzone.hidden = true
  fileRow.hidden = false
  options.hidden = false
  packageOption.hidden = !isZip

  // Hide same-format options (no point converting to same format)
  const gnButton = formatOptions.querySelector('[data-value="goodnotes"]')
  const ntbButton = formatOptions.querySelector('[data-value="ntb"]')
  if (gnButton) gnButton.hidden = isGoodnotes
  if (ntbButton) ntbButton.hidden = isNtb

  // Reset format if current selection is hidden
  if ((isGoodnotes && outputFormat === "goodnotes") || (isNtb && outputFormat === "ntb")) {
    outputFormat = "pdf"
    selectOption(formatOptions, outputFormat)
  }

  progressArea.hidden = true
  convertButton.hidden = false
  input.value = ""
  preloadedBytes = isZip || file.size > 0x7fffffff ? null : file.arrayBuffer()
  if (preloadedBytes) {
    preloadedBytes.catch((error) => {
      if (selectedFile === file) showError(error instanceof Error ? error.message : String(error))
    })
  }
}

async function convert() {
  if (!selectedFile || busy) return
  busy = true
  showError()
  convertButton.disabled = true
  input.disabled = true
  changeButton.disabled = true
  for (const button of document.querySelectorAll(".segment")) button.disabled = true
  progressArea.hidden = false
  progressLabel.textContent = "Starting"
  progressPercent.textContent = "0%"
  progressValue.style.width = "0%"
  progressTrack.setAttribute("aria-valuenow", "0")

  const progress = ({ label, pct }) => {
    progressLabel.textContent = label
    progressPercent.textContent = `${pct}%`
    progressValue.style.width = `${pct}%`
    progressTrack.setAttribute("aria-valuenow", String(pct))
  }

  try {
    const bytes = preloadedBytes ? await preloadedBytes : null
    const fmtCode = { pdf: 0, svg: 1, goodnotes: 2, ntb: 3 }[outputFormat] ?? 0
    const { files, meta } = await convertFile(
      selectedFile.name,
      selectedFile,
      bytes,
      fmtCode,
      outputPackaging === "zip",
      progress,
    )
    outputFiles = files
    savedMeta = meta

    // Show preview button if there are pages to preview
    if (previewButton && meta?.pages > 0) {
      previewButton.hidden = false
    }

    results.replaceChildren()
    for (const file of outputFiles) {
      const link = document.createElement("a")
      link.href = file.url
      link.download = file.name
      const name = document.createElement("span")
      name.textContent = outputFiles.length === 1 ? `Download ${outputFormat.toUpperCase()}` : file.name
      const size = document.createElement("small")
      size.textContent = formatSize(file.size)
      link.append(name, size)
      results.append(link)
    }
    results.hidden = outputFiles.length === 0
    progressArea.hidden = true
    convertButton.hidden = true

    if (outputFiles.length === 1) {
      const link = results.querySelector("a")
      link.click()
    }
  } catch (error) {
    progressArea.hidden = true
    convertButton.disabled = false
    showError(error instanceof Error ? error.message : String(error))
  } finally {
    busy = false
    input.disabled = false
    changeButton.disabled = false
    for (const button of document.querySelectorAll(".segment")) button.disabled = false
  }
}

input.addEventListener("change", () => selectFile(input.files?.[0]))
changeButton.addEventListener("click", reset)
convertButton.addEventListener("click", convert)
formatOptions.addEventListener("click", (event) => {
  const button = event.target.closest(".segment")
  if (!button || busy) return
  outputFormat = button.dataset.value
  selectOption(formatOptions, outputFormat)
})
packageOptions.addEventListener("click", (event) => {
  const button = event.target.closest(".segment")
  if (!button || busy) return
  outputPackaging = button.dataset.value
  selectOption(packageOptions, outputPackaging)
})

// Lazy preview: only load thumbnails when user clicks the button
previewButton?.addEventListener("click", async () => {
  if (busy || previewLoaded || !savedMeta) return
  busy = true
  previewButton.disabled = true
  previewButton.textContent = "Loading preview…"
  try {
    const { thumbs, loadMore } = await loadThumbnails(savedMeta)
    outputThumbs = thumbs
    loadMoreThumbs = loadMore
    previewLoaded = true
    if (thumbsEl && thumbs.length) {
      thumbsEl.replaceChildren()
      appendThumbs(thumbs)
      thumbsEl.hidden = false
      if (thumbsSection) thumbsSection.hidden = false
    }
    previewButton.textContent = "Preview"
    previewButton.hidden = true
  } catch (error) {
    showError(error instanceof Error ? error.message : String(error))
    previewButton.disabled = false
    previewButton.textContent = "Show Preview"
  } finally {
    busy = false
  }
})

dropzone.addEventListener("dragenter", (event) => {
  event.preventDefault()
  dropzone.classList.add("is-over")
})
dropzone.addEventListener("dragover", (event) => event.preventDefault())
dropzone.addEventListener("dragleave", () => dropzone.classList.remove("is-over"))
dropzone.addEventListener("drop", (event) => {
  event.preventDefault()
  dropzone.classList.remove("is-over")
  selectFile(event.dataTransfer?.files[0])
})
dropzone.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault()
    input.click()
  }
})

thumbsEl?.addEventListener("scroll", () => {
  if (!loadMoreThumbs || loadingMore || busy) return
  const { scrollLeft, scrollWidth, clientWidth } = thumbsEl
  if (scrollLeft + clientWidth < scrollWidth - 80) return
  loadingMore = true
  loadMoreThumbs().then((batch) => {
    if (batch.length) appendThumbs(batch)
    else loadMoreThumbs = null
  }).catch(() => {
    loadMoreThumbs = null
  }).finally(() => {
    loadingMore = false
  })
})

if ("requestIdleCallback" in window) {
  requestIdleCallback(() => prewarm(), { timeout: 1200 })
} else {
  setTimeout(() => prewarm(), 300)
}
