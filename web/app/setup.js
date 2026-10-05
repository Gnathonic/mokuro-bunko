// Setup: the role chooser (the OCR steps need the full build).
document.addEventListener('DOMContentLoaded', async () => {
  'use strict';
  try {
    const i = await App.info();
    if (!i.ocr_build) {
      document.querySelectorAll('[data-ocr]').forEach((a) => {
        a.removeAttribute('href');
        a.setAttribute('aria-disabled', 'true');
        a.style.opacity = '0.5';
        a.querySelector('.choice__text').textContent += ' (full build only)';
      });
    }
  } catch (_) { /* the links still work */ }
});
