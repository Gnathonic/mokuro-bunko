// Registration Page JavaScript

const API_URL = '/api/register';
const CONFIG_URL = '/api/register/config';

// DOM Elements
const form = document.getElementById('register-form');
const usernameInput = document.getElementById('username');
const passwordInput = document.getElementById('password');
const confirmPasswordInput = document.getElementById('confirm-password');
const inviteCodeInput = document.getElementById('invite-code');
const inviteGroup = document.getElementById('invite-group');
const submitBtn = document.getElementById('submit-btn');
const errorMessage = document.getElementById('error-message');
const successMessage = document.getElementById('success-message');
const disabledMessage = document.getElementById('disabled-message');
const pendingMessage = document.getElementById('pending-message');
const headerNav = document.getElementById('header-nav');

// Validation error elements
const usernameError = document.getElementById('username-error');
const passwordError = document.getElementById('password-error');
const confirmError = document.getElementById('confirm-error');
const inviteError = document.getElementById('invite-error');

// Registration mode (self, invite, approval, disabled)
let registrationMode = 'self';

function getSessionUser() {
    const userStr = sessionStorage.getItem('mokuro_user');
    if (!userStr) return null;
    try {
        return JSON.parse(userStr);
    } catch (_) {
        return null;
    }
}

async function updateNav() {
    if (!headerNav) return;
    if (window.renderMokuroHeaderNav) {
        await window.renderMokuroHeaderNav('register');
    }
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.href = '/';
}

// Initialize
document.addEventListener('DOMContentLoaded', async () => {
    updateNav();
    await loadConfig();
    setupValidation();
    form.addEventListener('submit', handleSubmit);
});

// Load registration configuration
async function loadConfig() {
    try {
        const response = await fetch(CONFIG_URL);
        if (response.ok) {
            const config = await response.json();
            registrationMode = config.mode || 'self';

            if (registrationMode === 'disabled') {
                showDisabled();
            } else if (registrationMode === 'invite') {
                showInviteField();
            }
        }
    } catch (error) {
        console.error('Failed to load config:', error);
        // Default to self registration mode
    }
}

// Show invite code field
function showInviteField() {
    inviteGroup.classList.add('visible');
    inviteCodeInput.required = true;
}

// Show registration disabled message
function showDisabled() {
    form.style.display = 'none';
    disabledMessage.style.display = 'block';
    document.querySelector('.login-link').style.display = 'none';
}

// Show pending approval message
function showPending() {
    form.style.display = 'none';
    pendingMessage.style.display = 'block';
    document.querySelector('.login-link').style.display = 'none';
}

// Show success message
function showSuccess() {
    form.style.display = 'none';
    successMessage.classList.add('visible');
}

// Show error message
function showError(message) {
    errorMessage.textContent = message;
    errorMessage.classList.add('visible');
}

// Hide error message
function hideError() {
    errorMessage.classList.remove('visible');
}

// Set field error
function setFieldError(element, errorElement, message) {
    element.classList.add('error');
    errorElement.textContent = message;
    errorElement.classList.add('visible');
}

// Clear field error
function clearFieldError(element, errorElement) {
    element.classList.remove('error');
    errorElement.classList.remove('visible');
}

// Setup real-time validation
function setupValidation() {
    usernameInput.addEventListener('input', validateUsername);
    passwordInput.addEventListener('input', validatePassword);
    confirmPasswordInput.addEventListener('input', validateConfirmPassword);
    inviteCodeInput.addEventListener('input', () => {
        clearFieldError(inviteCodeInput, inviteError);
    });
}

// Validate username
function validateUsername() {
    const value = usernameInput.value;
    clearFieldError(usernameInput, usernameError);

    if (value.length > 0 && value.length < 3) {
        setFieldError(usernameInput, usernameError, 'Username must be at least 3 characters');
        return false;
    }

    if (value.length > 32) {
        setFieldError(usernameInput, usernameError, 'Username must be at most 32 characters');
        return false;
    }

    if (value && !/^[a-zA-Z0-9_-]+$/.test(value)) {
        setFieldError(usernameInput, usernameError, 'Only letters, numbers, underscores, and hyphens allowed');
        return false;
    }

    return true;
}

// Validate password
function validatePassword() {
    const value = passwordInput.value;
    clearFieldError(passwordInput, passwordError);

    if (value.length > 0 && value.length < 8) {
        setFieldError(passwordInput, passwordError, 'Password must be at least 8 characters');
        return false;
    }

    // Also validate confirm password if it has a value
    if (confirmPasswordInput.value) {
        validateConfirmPassword();
    }

    return true;
}

// Validate confirm password
function validateConfirmPassword() {
    const value = confirmPasswordInput.value;
    clearFieldError(confirmPasswordInput, confirmError);

    if (value && value !== passwordInput.value) {
        setFieldError(confirmPasswordInput, confirmError, 'Passwords do not match');
        return false;
    }

    return true;
}

// Handle form submission
async function handleSubmit(event) {
    event.preventDefault();
    hideError();

    // Validate all fields
    const isUsernameValid = validateUsername();
    const isPasswordValid = validatePassword();
    const isConfirmValid = validateConfirmPassword();

    if (!isUsernameValid || !isPasswordValid || !isConfirmValid) {
        return;
    }

    // Disable submit button
    submitBtn.disabled = true;
    submitBtn.textContent = 'Creating Account...';

    try {
        const body = {
            username: usernameInput.value,
            password: passwordInput.value,
        };

        if (registrationMode === 'invite' && inviteCodeInput.value) {
            body.invite_code = inviteCodeInput.value;
        }

        const response = await fetch(API_URL, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify(body),
        });

        const data = await response.json();

        if (response.ok) {
            if (data.status === 'pending') {
                showPending();
            } else {
                showSuccess();
            }
        } else {
            // Handle specific errors
            if (response.status === 409) {
                setFieldError(usernameInput, usernameError, 'Username already taken');
            } else if (response.status === 400 && data.error) {
                if (data.error.toLowerCase().includes('invite')) {
                    setFieldError(inviteCodeInput, inviteError, data.error);
                } else if (data.error.toLowerCase().includes('username')) {
                    setFieldError(usernameInput, usernameError, data.error);
                } else if (data.error.toLowerCase().includes('password')) {
                    setFieldError(passwordInput, passwordError, data.error);
                } else {
                    showError(data.error);
                }
            } else if (response.status === 403) {
                showError('Registration is not available.');
            } else {
                showError(data.error || 'Registration failed. Please try again.');
            }
        }
    } catch (error) {
        console.error('Registration error:', error);
        showError('Network error. Please check your connection and try again.');
    } finally {
        submitBtn.disabled = false;
        submitBtn.textContent = 'Create Account';
    }
}
