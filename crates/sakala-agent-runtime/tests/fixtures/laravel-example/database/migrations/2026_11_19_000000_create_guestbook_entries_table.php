<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    /**
     * Sakala runs this through the release command `php artisan migrate --force`,
     * once per deployment, before the new version takes traffic.
     */
    public function up(): void
    {
        Schema::create('guestbook_entries', function (Blueprint $table) {
            $table->id();
            $table->string('name', 60);
            $table->string('message', 280);
            $table->timestamps();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('guestbook_entries');
    }
};
