// Login JavaScript

const form = document.getElementById('login-form');
const errorMsg = document.getElementById('error-message');
const submitBtn = document.getElementById('submit-btn');
const headerNav = document.getElementById('header-nav');

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
        await window.renderMokuroHeaderNav('login');
    }
}

function logout() {
    sessionStorage.removeItem('mokuro_auth');
    sessionStorage.removeItem('mokuro_user');
    window.location.href = '/';
}

updateNav();

form.addEventListener('submit', async (e) => {
    e.preventDefault();
    
    const username = document.getElementById('username').value;
    const password = document.getElementById('password').value;
    
    errorMsg.textContent = '';
    submitBtn.disabled = true;
    submitBtn.textContent = 'Signing in...';
    
    try {
        const response = await fetch('/login/api/check', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({ username, password })
        });
        
        const data = await response.json();
        
        if (!response.ok) {
            throw new Error(data.error || 'Authentication failed');
        }
        
        // Store credentials for WebDAV requests
        const credentials = btoa(username + ':' + password);
        sessionStorage.setItem('mokuro_auth', credentials);
        sessionStorage.setItem('mokuro_user', JSON.stringify(data.user));
        
        // Redirect to home
        window.location.href = '/';
        
    } catch (err) {
        errorMsg.textContent = err.message;
        submitBtn.disabled = false;
        submitBtn.textContent = 'Sign In';
    }
});
