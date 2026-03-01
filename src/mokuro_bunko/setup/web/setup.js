// Setup Wizard JavaScript

const steps = ['step-welcome', 'step-admin', 'step-registration', 'step-done'];
let currentStep = 0;

// Check if setup is needed
document.addEventListener('DOMContentLoaded', async () => {
    try {
        const resp = await fetch('/setup/api/status');
        const data = await resp.json();
        if (!data.needs_setup) {
            // Already set up, redirect to home
            window.location.href = '/';
        }
    } catch (err) {
        // Continue with setup
    }
});

function showStep(index) {
    steps.forEach((id, i) => {
        const el = document.getElementById(id);
        if (i === index) {
            el.classList.add('active');
        } else {
            el.classList.remove('active');
        }
    });

    // Update progress dots
    document.querySelectorAll('.setup-dot').forEach((dot, i) => {
        if (i <= index) {
            dot.classList.add('active');
        } else {
            dot.classList.remove('active');
        }
    });

    currentStep = index;
}

function nextStep() {
    if (currentStep < steps.length - 1) {
        showStep(currentStep + 1);
    }
}

function prevStep() {
    if (currentStep > 0) {
        showStep(currentStep - 1);
    }
}

function validateAdmin() {
    const username = document.getElementById('admin-username').value.trim();
    const password = document.getElementById('admin-password').value;
    const confirm = document.getElementById('admin-password-confirm').value;
    const errorEl = document.getElementById('admin-error');

    // Validate username
    if (!username || !/^[a-zA-Z0-9_-]{3,32}$/.test(username)) {
        errorEl.textContent = 'Username must be 3-32 characters (letters, numbers, _, -)';
        errorEl.style.display = '';
        return;
    }

    // Validate password
    if (!password || password.length < 8) {
        errorEl.textContent = 'Password must be at least 8 characters';
        errorEl.style.display = '';
        return;
    }

    // Confirm password match
    if (password !== confirm) {
        errorEl.textContent = 'Passwords do not match';
        errorEl.style.display = '';
        return;
    }

    errorEl.style.display = 'none';
    nextStep();
}

async function completeSetup() {
    const username = document.getElementById('admin-username').value.trim();
    const password = document.getElementById('admin-password').value;
    const regMode = document.querySelector('input[name="reg-mode"]:checked').value;

    const payload = {
        admin: { username, password },
        registration: { mode: regMode },
    };

    try {
        const resp = await fetch('/setup/api/complete', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload),
        });

        const data = await resp.json();

        if (resp.ok) {
            // Store auth for admin panel access
            const auth = btoa(username + ':' + password);
            sessionStorage.setItem('mokuro_auth', auth);
            showStep(3); // Done step
        } else {
            const errorEl = document.getElementById('admin-error');
            errorEl.textContent = data.error || 'Setup failed';
            errorEl.style.display = '';
            showStep(1); // Back to admin step
        }
    } catch (err) {
        const errorEl = document.getElementById('admin-error');
        errorEl.textContent = 'Connection error: ' + err.message;
        errorEl.style.display = '';
        showStep(1);
    }
}
