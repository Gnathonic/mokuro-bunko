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

async function logout() {
    await window.mokuroAuth.signOut();
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
        // The password is checked once and traded for a token; it is not kept.
        await window.mokuroAuth.signIn(username, password);

        // Redirect to home
        window.location.href = '/';
        
    } catch (err) {
        errorMsg.textContent = err.message;
        submitBtn.disabled = false;
        submitBtn.textContent = 'Sign In';
    }
});
