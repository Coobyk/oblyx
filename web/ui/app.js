import { convertFile, formatSize, prewarm } from "./engine.js"

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

let selectedFile = null
let preloadedBytes = null
let outputFiles = []
let busy = false
let outputFormat = "pdf"
let outputPackaging = "zip"

function selectOption(group, value) {
  for (const button of group.querySelectorAll(".segment")) {
    const selected = button.dataset.value === value
    button.classList.toggle("is-active", selected)
    button.setAttribute("aria-pressed", String(selected))
  }
}

function clearOutputs() {
  for (const file of outputFiles) URL.revokeObjectURL(file.url)
  outputFiles = []
  results.replaceChildren()
  results.hidden = true
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
  if (!isZip && !/\.goodnotes$/i.test(file.name)) {
    showError("Choose a .goodnotes file or .zip archive.")
    return
  }

  clearOutputs()
  showError()
  selectedFile = file
  fileName.textContent = file.name
  fileSize.textContent = `${formatSize(file.size)} · ${isZip ? "ZIP archive" : "Goodnotes notebook"}`
  dropzone.hidden = true
  fileRow.hidden = false
  options.hidden = false
  packageOption.hidden = !isZip
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
    outputFiles = await convertFile(
      selectedFile.name,
      selectedFile,
      bytes,
      outputFormat === "pdf" ? 0 : 1,
      outputPackaging === "zip",
      progress,
    )

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

if ("requestIdleCallback" in window) {
  requestIdleCallback(() => prewarm(), { timeout: 1200 })
} else {
  setTimeout(() => prewarm(), 300)
}
