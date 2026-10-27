// Run from beside the page: marks itself, then reaches for the network,
// which the preview's policy refuses.
document.getElementById("script").textContent = "The script beside the page ran.";
document.getElementById("script").className = "ran";
const network = document.getElementById("network");
fetch("http://127.0.0.1:8765/fetch")
  .then(() => { network.textContent = "The fetch reached the network."; })
  .catch(() => {
    network.textContent = "The fetch to the network was refused.";
    network.className = "refused";
  });
